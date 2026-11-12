//! The vault's own thread: the only place that touches the database, derives keys or syncs.
//!
//! The main thread sends a [`Request`] and carries on; what comes of it arrives as an
//! [`AppEvent::Vault`]. Requests run one at a time, in order.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use rmx_serial::serial::{DataBits, FlowControl, Parity, SerialConfig, StopBits};
use rmx_ssh::{AuthMethod, HostConfig, SecretString};
use rmx_ui::widgets::{
    AuthKind, ConnectionDraft, NodeKind, RdpQuality, RdpSecurity, ScaleChoice, SessionDot,
    SettingsDraft, SnippetRow, SyncDraft, SyncKind, TreeModel, TreeNode, VncColors,
};
use rmx_ui::{NodeId, Protocol, ThemeMode};
use rmx_vault::{
    Connection, Credential, Credentials, Error, Folder, Id, KdfParams, KnownHost, LocalPathBackend,
    S3Backend, S3Config, Secret, SftpBackend, SharedSetting, Snippet, SshKey, SyncBackend,
    SyncReport, Tunnel, Vault, VaultKind, WebDavBackend, WebDavConfig,
};
use tokio::runtime::Handle;
use tokio::sync::oneshot;
use winit::event_loop::EventLoopProxy;

use crate::app::AppEvent;
use crate::background::{AppState, Layout, Paths, off_main};
use crate::connect::{ConnectPlan, Endpoint, sync_verifier};
use crate::options;
use crate::sftp_files::{SftpFiles, SftpTarget};

/// How long after a save it syncs, so that a burst of saves is one sync.
const SYNC_AFTER_SAVE: Duration = Duration::from_millis(1500);
/// The auto-lock of a vault that has not set one.
const DEFAULT_AUTO_LOCK_MINUTES: u64 = 15;
/// Name of the folder imported hosts go in.
const IMPORT_FOLDER: &str = "SSH config";
/// What is said if a database with no password is asked for without the user having agreed.
const PLAIN_NOT_AGREED: &str = "Confirm that it will not be encrypted, or enter a master password";
/// The local state's name for the folded folders.
const KEY_COLLAPSED: &str = "tree.collapsed";
/// The keys of a connection's options that its form owns.
const OPTION_SCALE: &str = "scale";
const OPTION_RESIZE: &str = "resize";
const OPTION_SECURITY: &str = "security";
const OPTION_QUALITY: &str = "quality";
const OPTION_COLORS: &str = "colors";
const OPTION_FTPS: &str = "ftps";
const OPTION_DOMAIN: &str = "domain";
const OPTION_BAUD: &str = "baud";
const OPTION_DATA_BITS: &str = "data_bits";
const OPTION_PARITY: &str = "parity";
const OPTION_STOP_BITS: &str = "stop_bits";
const OPTION_FLOW: &str = "flow";
const OPTION_RENDEZVOUS: &str = "rendezvous";
const OPTION_SERVER_KEY: &str = "server_key";

/// Something for the vault's thread to do.
pub enum Request {
    Create {
        password: Secret,
        key_file: Option<PathBuf>,
        plain_ok: bool,
    },
    Unlock {
        password: Secret,
        key_file: Option<PathBuf>,
    },
    Join {
        password: Secret,
        key_file: Option<PathBuf>,
        sync: Box<SyncDraft>,
        plain_ok: bool,
    },
    /// Re-encrypt the vault with a new master password and key file, and the synced copy too.
    /// The current password must be right; the current key file is the one it was opened with.
    ChangePassword {
        current: Secret,
        new: Secret,
        key_file: Option<PathBuf>,
    },
    /// The synced copy was re-keyed on another device: take on its password and key file.
    AdoptPassword {
        password: Secret,
        key_file: Option<PathBuf>,
    },
    Lock,
    /// Close the vault and go to the one at this path, which need not exist yet.
    Switch(String),
    /// Move this vault's file to this path, or into this folder, and carry on from there.
    Move(String),
    /// The user did something: the auto-lock starts over.
    Touch,
    SaveConnection(Box<ConnectionDraft>),
    EditConnection(u64),
    Duplicate(u64),
    DeleteConnection(u64),
    NewFolder {
        parent: Option<u64>,
        name: String,
    },
    RenameFolder {
        node: u64,
        name: String,
    },
    DeleteFolder(u64),
    FolderToggled {
        node: u64,
        expanded: bool,
    },
    LoadSettings {
        theme: ThemeMode,
    },
    SaveSettings(Box<SettingsDraft>),
    SyncNow,
    ImportSshConfig,
    Plan {
        node: u64,
        reply: oneshot::Sender<Result<ConnectPlan, String>>,
    },
    KnownHosts {
        host: String,
        port: u16,
        reply: oneshot::Sender<Result<Vec<KnownHost>, String>>,
    },
    TrustHost(KnownHost),
    SaveSnippet {
        id: Option<u64>,
        name: String,
        body: String,
    },
    DeleteSnippet(u64),
    SaveLayout(Layout),
    /// How the windows are laid out, as it goes in the state file.
    SaveSession(String),
    Shutdown(Layout, Option<String>, mpsc::Sender<()>),
}

/// How to reach the vault's thread. Sending never waits.
#[derive(Clone)]
pub struct VaultHandle(mpsc::Sender<Request>);

impl VaultHandle {
    pub fn send(&self, request: Request) {
        // Fails only while the application is ending.
        let _ = self.0.send(request);
    }
}

/// How a node of the tree is known to the main thread.
pub struct NodeInfo {
    pub name: String,
    pub kind: NodeKind,
    /// Where it connects to; empty for folders.
    pub host: String,
}

/// The tree, built off the main thread so that showing it costs nothing there.
pub struct TreeUpdate {
    pub model: TreeModel,
    pub nodes: HashMap<u64, NodeInfo>,
    /// The snippets, sorted by name: they change whenever the rest can.
    pub snippets: Vec<SnippetRow>,
}

/// What a new database is to be protected with.
enum Protection {
    Encrypted(Credentials),
    Plain,
}

impl Protection {
    /// What the form said. Having nothing to protect it with counts only if the user agreed to
    /// that, whichever way the request got here.
    fn asked_for(
        password: Secret,
        key_file: Option<PathBuf>,
        plain_ok: bool,
    ) -> Result<Self, String> {
        if password.is_empty() && key_file.is_none() {
            return if plain_ok { Ok(Self::Plain) } else { Err(PLAIN_NOT_AGREED.to_owned()) };
        }
        let credentials = Credentials::new(password);
        Ok(Self::Encrypted(match key_file {
            Some(path) => credentials.with_key_file(path),
            None => credentials,
        }))
    }
}

/// Which screen the vault calls for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Phase {
    /// No vault yet, at this path.
    Setup(PathBuf),
    Locked(PathBuf),
    /// With the key file it was opened with, if any.
    Unlocked {
        unencrypted: bool,
        key_file: Option<PathBuf>,
    },
}

/// What sync is doing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SyncState {
    Off,
    Running,
    Done(String),
    Failed(String),
}

/// What the main thread hears from the vault's thread.
pub enum VaultEvent {
    Phase(Phase),
    /// A create, unlock or join failed.
    OpenFailed(String),
    Tree(Box<TreeUpdate>),
    Sync(SyncState),
    /// Something to tell the user, with whether it is bad news.
    Notice(String, bool),
    /// How a master password change went, to be shown in a dialog: title, detail, bad news.
    PasswordOutcome(String, String, bool),
    Settings(Box<SettingsDraft>),
    Draft(Box<ConnectionDraft>),
    /// The connection was saved from "save and connect".
    Saved {
        node: u64,
        connect: bool,
    },
    /// Which vault this is, and the others this device knows.
    Vaults {
        current: PathBuf,
        others: Vec<PathBuf>,
    },
    /// The synced copy has another master password: ask for it.
    RemoteKeyChanged,
}

/// The number the tree knows a row by, stable across reloads and devices.
fn node_number(id: Id) -> u64 {
    let mut hasher = DefaultHasher::new();
    id.to_string().hash(&mut hasher);
    hasher.finish()
}

fn protocol_name(protocol: Protocol) -> String {
    protocol.label().to_lowercase()
}

fn parse_protocol(name: &str) -> Protocol {
    Protocol::ALL
        .into_iter()
        .find(|protocol| protocol.label().eq_ignore_ascii_case(name))
        .unwrap_or(Protocol::Ssh)
}

fn non_empty(text: &str) -> Option<String> {
    let text = text.trim();
    (!text.is_empty()).then(|| text.to_owned())
}

fn expand_home(path: &str) -> PathBuf {
    match (path.strip_prefix("~/"), std::env::home_dir()) {
        (Some(rest), Some(home)) => home.join(rest),
        _ => PathBuf::from(path),
    }
}

/// A private key kept out of every file: the SSH library reads it through `/proc`.
#[cfg(target_os = "linux")]
pub struct KeyHold {
    fd: i32,
}

/// A private key for the SSH library to read. Only Linux has a file that is not on disk, so here
/// it is a file of the user's own in the temporary directory, removed when the session is done.
#[cfg(not(target_os = "linux"))]
pub struct KeyHold {
    path: PathBuf,
}

#[cfg(not(target_os = "linux"))]
impl KeyHold {
    fn new(key: &Secret) -> Result<Self, String> {
        use std::io::Write;

        let mut name = [0u8; 8];
        ring::rand::SecureRandom::fill(&ring::rand::SystemRandom::new(), &mut name)
            .map_err(|_| "could not hold the key".to_owned())?;
        let name: String = name.iter().map(|byte| format!("{byte:02x}")).collect();
        let hold = Self { path: std::env::temp_dir().join(format!("rmx-key-{name}")) };
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
        let written =
            options.open(&hold.path).and_then(|mut file| file.write_all(key.expose().as_bytes()));
        written.map_err(|err| format!("could not hold the key: {err}"))?;
        Ok(hold)
    }

    fn path(&self) -> PathBuf {
        self.path.clone()
    }
}

#[cfg(not(target_os = "linux"))]
impl Drop for KeyHold {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

#[cfg(target_os = "linux")]
impl KeyHold {
    fn new(key: &Secret) -> Result<Self, String> {
        // Closed on exec, so that a local shell does not inherit the key.
        let fd = unsafe { libc::memfd_create(c"rmx-key".as_ptr(), libc::MFD_CLOEXEC) };
        if fd < 0 {
            return Err(format!("could not hold the key: {}", std::io::Error::last_os_error()));
        }
        let hold = Self { fd };
        let bytes = key.expose().as_bytes();
        let written = unsafe { libc::write(fd, bytes.as_ptr().cast(), bytes.len()) };
        if written != bytes.len() as isize {
            return Err("could not hold the key".to_owned());
        }
        Ok(hold)
    }

    fn path(&self) -> PathBuf {
        PathBuf::from(format!("/proc/self/fd/{}", self.fd))
    }
}

#[cfg(target_os = "linux")]
impl Drop for KeyHold {
    fn drop(&mut self) {
        unsafe { libc::close(self.fd) };
    }
}

/// A saved sync backend.
#[derive(Clone)]
struct SyncSettings {
    draft: SyncDraft,
    on_save: bool,
    interval: Option<Duration>,
}

fn minutes(text: &str) -> Option<Duration> {
    text.trim().parse::<u64>().ok().filter(|&m| m > 0).map(|m| Duration::from_secs(m * 60))
}

struct Actor {
    runtime: Handle,
    proxy: EventLoopProxy<AppEvent>,
    state: Option<AppState>,
    path: PathBuf,
    vault: Option<Vault>,
    /// The key file the open vault was opened with, which a password change keeps unless told.
    key_file: Option<PathBuf>,
    /// The user has been asked for the synced copy's new password since the vault was opened.
    asked_remote_key: bool,
    /// The last tree's numbers for its rows.
    ids: HashMap<u64, Id>,
    collapsed: HashSet<Id>,
    /// When the user last did something.
    active: Instant,
    auto_lock: Option<Duration>,
    sync: Option<SyncSettings>,
    sync_due: Option<Instant>,
    next_interval: Option<Instant>,
}

/// Start the vault's thread.
pub fn spawn(
    runtime: Handle,
    proxy: EventLoopProxy<AppEvent>,
    paths: Paths,
    state: Option<AppState>,
) -> std::io::Result<VaultHandle> {
    let (sender, receiver) = mpsc::channel();
    let path = state.as_ref().and_then(AppState::last_vault).unwrap_or(paths.default_vault.clone());
    let actor = Actor {
        runtime,
        proxy,
        state,
        path,
        vault: None,
        key_file: None,
        asked_remote_key: false,
        ids: HashMap::new(),
        collapsed: HashSet::new(),
        active: Instant::now(),
        auto_lock: None,
        sync: None,
        sync_due: None,
        next_interval: None,
    };
    thread::Builder::new().name("rmx-vault".into()).spawn(move || actor.run(&receiver))?;
    Ok(VaultHandle(sender))
}

impl Actor {
    fn run(mut self, requests: &mpsc::Receiver<Request>) {
        // Sessions opened for syncing need the runtime even as they are dropped.
        let runtime = self.runtime.clone();
        let _context = runtime.enter();

        self.first_open();
        self.announce_vaults();
        loop {
            let request = match self.next_deadline() {
                Some(at) => requests.recv_timeout(at.saturating_duration_since(Instant::now())),
                None => requests.recv().map_err(|_| RecvTimeoutError::Disconnected),
            };
            match request {
                Ok(Request::Shutdown(layout, session, done)) => {
                    self.save_layout(&layout);
                    if let Some(session) = session {
                        self.save_session(&session);
                    }
                    let _ = done.send(());
                    return;
                },
                Ok(request) => self.handle(request),
                Err(RecvTimeoutError::Timeout) => self.tick(),
                Err(RecvTimeoutError::Disconnected) => return,
            }
        }
    }

    fn send(&self, event: VaultEvent) {
        let _ = self.proxy.send_event(AppEvent::Vault(event));
    }

    fn notice(&self, text: impl Into<String>, bad: bool) {
        self.send(VaultEvent::Notice(text.into(), bad));
    }

    fn password_outcome(&self, done: &str, failed: &str, outcome: Result<String, String>) {
        let event = match outcome {
            Ok(message) => VaultEvent::PasswordOutcome(done.to_owned(), message, false),
            Err(err) => VaultEvent::PasswordOutcome(failed.to_owned(), err, true),
        };
        self.send(event);
    }

    fn save_session(&self, session: &str) {
        if let Some(state) = &self.state {
            state.save_session(session);
        }
    }

    fn save_layout(&self, layout: &Layout) {
        if let Some(state) = &self.state {
            state.save_layout(layout);
        }
    }

    /// Say which vaults there are to switch between. One whose file has gone is not offered.
    fn announce_vaults(&self) {
        let known = self.state.as_ref().map_or_else(Vec::new, AppState::recent_vaults);
        let others =
            known.into_iter().filter(|path| *path != self.path && path.is_file()).collect();
        self.send(VaultEvent::Vaults { current: self.path.clone(), others });
    }

    /// Close this vault and go to the one at `typed`. Sessions it started keep running.
    fn switch(&mut self, typed: &str) {
        let expanded = expand_home(typed.trim());
        let Ok(path) = std::path::absolute(&expanded) else { return };
        if path.is_dir() {
            let reason = format!("{} is a folder", path.display());
            self.send(VaultEvent::OpenFailed(reason.clone()));
            self.notice(reason, true);
            return;
        }
        if path == self.path && self.is_open() {
            return;
        }
        if self.is_open() && self.sync_due.take().is_some() {
            self.run_sync(false);
        }
        self.go_to(path);
    }

    /// Put this vault's file, and the files beside it, at `typed`: a path, or a folder to keep its
    /// name in. Nothing already there is replaced. The vault is closed for it, which is what
    /// makes the file whole.
    fn relocate(&mut self, typed: &str) -> Result<Option<PathBuf>, String> {
        let text = |err: std::io::Error| err.to_string();
        let mut path = std::path::absolute(expand_home(typed.trim())).map_err(text)?;
        if path.is_dir() {
            path.push(self.path.file_name().ok_or("The vault has no file name")?);
        }
        if path == self.path {
            return Ok(None);
        }
        if !self.path.is_file() {
            return Err("There is no vault file to move yet".to_owned());
        }
        if path.exists() {
            return Err(format!("{} already exists", path.display()));
        }
        if self.is_open() && self.sync_due.take().is_some() {
            self.run_sync(false);
        }
        self.vault = None;
        move_vault(&self.path, &path)?;
        Ok(Some(path))
    }

    /// Leave the vault that is open for the one at `path`.
    fn go_to(&mut self, path: PathBuf) {
        self.vault = None;
        self.key_file = None;
        self.path = path;
        if self.path.is_file()
            && let Some(state) = &self.state
        {
            state.set_last_vault(&self.path);
        }
        self.ids.clear();
        self.collapsed.clear();
        self.auto_lock = None;
        self.sync = None;
        self.sync_due = None;
        self.next_interval = None;
        self.first_open();
        self.announce_vaults();
    }

    /// What to show with nothing open. A database that cannot be told apart from nothing there
    /// must not be offered up to be overwritten, so anything unreadable counts as locked.
    fn resting_phase(&self) -> Phase {
        match VaultKind::of_file(&self.path) {
            Ok(None) => Phase::Setup(self.path.clone()),
            _ => Phase::Locked(self.path.clone()),
        }
    }

    /// Open what needs nothing from the user, or say what the user has to do first.
    fn first_open(&mut self) {
        if !matches!(VaultKind::of_file(&self.path), Ok(Some(VaultKind::Plain))) {
            self.send(VaultEvent::Phase(self.resting_phase()));
            return;
        }
        match Vault::open_plain(&self.path) {
            Ok(vault) => self.opened(vault, None),
            Err(err) => {
                self.send(VaultEvent::Phase(self.resting_phase()));
                self.send(VaultEvent::OpenFailed(err.to_string()));
            },
        }
    }

    fn is_open(&mut self) -> bool {
        self.vault.as_mut().is_some_and(|vault| !vault.is_locked())
    }

    fn next_deadline(&mut self) -> Option<Instant> {
        if !self.is_open() {
            return None;
        }
        let lock = self.auto_lock.map(|limit| self.active + limit);
        [lock, self.sync_due, self.next_interval].into_iter().flatten().min()
    }

    fn tick(&mut self) {
        let now = Instant::now();
        if self.auto_lock.is_some_and(|limit| self.active + limit <= now) {
            self.lock();
            return;
        }
        if self.sync_due.is_some_and(|due| due <= now) {
            self.sync_due = None;
            self.run_sync(false);
        }
        if self.next_interval.is_some_and(|due| due <= now) {
            self.schedule_interval();
            self.run_sync(false);
        }
    }

    fn schedule_interval(&mut self) {
        self.next_interval =
            self.sync.as_ref().and_then(|sync| sync.interval).map(|every| Instant::now() + every);
    }

    fn lock(&mut self) {
        if self.vault.as_ref().is_some_and(|vault| vault.kind() == VaultKind::Plain) {
            return;
        }
        if let Some(vault) = &mut self.vault {
            vault.lock();
        }
        self.sync_due = None;
        self.next_interval = None;
        self.send(VaultEvent::Phase(self.resting_phase()));
    }

    fn handle(&mut self, request: Request) {
        match request {
            Request::Create { password, key_file, plain_ok } => {
                self.create(password, key_file, plain_ok)
            },
            Request::Unlock { password, key_file } => self.unlock(password, key_file),
            Request::Join { password, key_file, sync, plain_ok } => {
                self.join(password, key_file, *sync, plain_ok);
            },
            Request::ChangePassword { current, new, key_file } => {
                let outcome = self.change_password(current, new, key_file);
                self.password_outcome(
                    "Master password changed",
                    "Master password not changed",
                    outcome,
                );
            },
            Request::AdoptPassword { password, key_file } => {
                let outcome = self.adopt_password(password, key_file);
                self.password_outcome(
                    "New master password in use",
                    "New master password not used",
                    outcome,
                );
            },
            Request::Lock => self.lock(),
            Request::Switch(path) => self.switch(&path),
            Request::Move(path) => match self.relocate(&path) {
                Ok(Some(path)) => {
                    let moved = format!("Vault moved to {}", path.display());
                    self.go_to(path);
                    self.notice(moved, false);
                },
                Ok(None) => {},
                Err(reason) => self.notice(reason, true),
            },
            Request::Touch => self.active = Instant::now(),
            Request::SaveLayout(layout) => self.save_layout(&layout),
            Request::SaveSession(session) => self.save_session(&session),
            Request::Shutdown(..) => (),
            Request::Plan { node, reply } => {
                let _ = reply.send(self.plan(node));
            },
            Request::KnownHosts { host, port, reply } => {
                let rows = self.known_hosts(&host, port);
                let _ = reply.send(rows);
            },
            Request::TrustHost(row) => {
                if let Some(vault) = &mut self.vault
                    && let Err(err) = vault.put(&row)
                {
                    self.notice(format!("Could not record the host key: {err}"), true);
                }
            },
            Request::FolderToggled { node, expanded } => self.folder_toggled(node, expanded),
            Request::LoadSettings { theme } => match self.load_settings(theme) {
                Ok(draft) => self.send(VaultEvent::Settings(Box::new(draft))),
                Err(err) => self.notice(err, true),
            },
            Request::SaveSettings(draft) => {
                if let Err(err) = self.save_settings(&draft) {
                    self.notice(err, true);
                } else {
                    self.notice("Settings saved", false);
                }
            },
            Request::SyncNow => {
                self.asked_remote_key = false;
                self.run_sync(true);
            },
            Request::EditConnection(node) => match self.draft_of(node) {
                Ok(draft) => self.send(VaultEvent::Draft(Box::new(draft))),
                Err(err) => self.notice(err, true),
            },
            Request::SaveConnection(draft) => {
                let connect = draft.editing.is_none();
                match self.save_connection(&draft) {
                    Ok(id) => {
                        self.changed();
                        self.send(VaultEvent::Saved { node: node_number(id), connect });
                    },
                    Err(err) => self.notice(err, true),
                }
            },
            other => match self.mutate(other) {
                Ok(Some(message)) => {
                    self.changed();
                    self.notice(message, false);
                },
                Ok(None) => self.changed(),
                Err(err) => self.notice(err, true),
            },
        }
    }

    fn mutate(&mut self, request: Request) -> Result<Option<String>, String> {
        match request {
            Request::SaveSnippet { id, name, body } => {
                self.save_snippet(id, name, body).map(|()| None)
            },
            Request::DeleteSnippet(id) => self.delete_snippet(id).map(|()| None),
            Request::Duplicate(node) => self.duplicate(node).map(|()| None),
            Request::DeleteConnection(node) => self.delete_connection(node).map(|()| None),
            Request::NewFolder { parent, name } => {
                let parent_id = parent.and_then(|node| self.ids.get(&node).copied());
                let folder = Folder { parent_id, name, sort_order: 0, ..Default::default() };
                self.open()?.put(&folder).map_err(text)?;
                Ok(None)
            },
            Request::RenameFolder { node, name } => {
                let id = self.id_of(node)?;
                let vault = self.open()?;
                let folder = vault.get::<Folder>(id).map_err(text)?.ok_or("no such folder")?;
                vault.put(&Folder { name, ..folder }).map_err(text)?;
                Ok(None)
            },
            Request::DeleteFolder(node) => {
                let id = self.id_of(node)?;
                self.open()?.delete::<Folder>(id).map_err(text)?;
                Ok(None)
            },
            Request::ImportSshConfig => self.import_ssh_config().map(Some),
            _ => Ok(None),
        }
    }

    fn open(&mut self) -> Result<&mut Vault, String> {
        self.vault.as_mut().ok_or_else(|| "the vault is locked".to_owned())
    }

    fn id_of(&self, node: u64) -> Result<Id, String> {
        self.ids.get(&node).copied().ok_or_else(|| "that item no longer exists".to_owned())
    }

    fn credentials(password: Secret, key_file: Option<PathBuf>) -> Credentials {
        let credentials = Credentials::new(password);
        match key_file {
            Some(path) => credentials.with_key_file(path),
            None => credentials,
        }
    }

    fn create(&mut self, password: Secret, key_file: Option<PathBuf>, plain_ok: bool) {
        let protection = match Protection::asked_for(password, key_file.clone(), plain_ok) {
            Ok(protection) => protection,
            Err(reason) => {
                self.send(VaultEvent::OpenFailed(reason));
                return;
            },
        };
        if let Some(dir) = self.path.parent()
            && let Err(err) = std::fs::create_dir_all(dir)
        {
            self.send(VaultEvent::OpenFailed(err.to_string()));
            return;
        }
        let created = match &protection {
            Protection::Encrypted(credentials) => {
                off_main("argon2");
                Vault::create(&self.path, credentials, KdfParams::RECOMMENDED)
            },
            Protection::Plain => Vault::create_plain(&self.path),
        };
        match created {
            Ok(vault) => self.opened(vault, key_file),
            Err(err) => self.send(VaultEvent::OpenFailed(err.to_string())),
        }
    }

    fn unlock(&mut self, password: Secret, key_file: Option<PathBuf>) {
        off_main("argon2");
        let credentials = Self::credentials(password, key_file.clone());
        let result = match self.vault.take() {
            Some(mut vault) => match vault.unlock(&credentials) {
                Ok(()) => Ok(vault),
                Err(err) => {
                    self.vault = Some(vault);
                    Err(err)
                },
            },
            None => Vault::open(&self.path, &credentials),
        };
        match result {
            Ok(vault) => self.opened(vault, key_file),
            Err(err) => self.send(VaultEvent::OpenFailed(err.to_string())),
        }
    }

    fn join(
        &mut self,
        password: Secret,
        key_file: Option<PathBuf>,
        sync: SyncDraft,
        plain_ok: bool,
    ) {
        let protection = match Protection::asked_for(password, key_file.clone(), plain_ok) {
            Ok(protection) => protection,
            Err(reason) => {
                self.send(VaultEvent::OpenFailed(reason));
                return;
            },
        };
        if let Some(dir) = self.path.parent()
            && let Err(err) = std::fs::create_dir_all(dir)
        {
            self.send(VaultEvent::OpenFailed(err.to_string()));
            return;
        }
        let (backend, trusted) = match self.backend(&sync, Vec::new(), true) {
            Ok(built) => built,
            Err(err) => {
                self.send(VaultEvent::OpenFailed(err));
                return;
            },
        };
        let joined = match &protection {
            Protection::Encrypted(credentials) => {
                off_main("argon2");
                Vault::join(&self.path, credentials, backend.as_ref())
            },
            Protection::Plain => Vault::join_plain(&self.path, backend.as_ref()),
        };
        match joined {
            Ok(mut vault) => {
                drop(backend);
                for row in
                    trusted.lock().map(|mut rows| std::mem::take(&mut *rows)).unwrap_or_default()
                {
                    let _ = vault.put(&row);
                }
                let settings = SyncSettings { draft: sync, on_save: true, interval: None };
                if let Err(err) = store_sync_settings(&mut vault, &settings, "") {
                    self.notice(format!("Could not save the sync settings: {err}"), true);
                }
                self.opened(vault, key_file);
            },
            Err(Error::KindMismatch { remote: VaultKind::Encrypted, .. }) => {
                self.send(VaultEvent::OpenFailed(
                    "The synced copy is encrypted: enter its master password".to_owned(),
                ));
            },
            Err(Error::KindMismatch { remote: VaultKind::Plain, .. }) => {
                self.send(VaultEvent::OpenFailed(
                    "The synced copy has no master password: leave it empty".to_owned(),
                ));
            },
            Err(err) => self.send(VaultEvent::OpenFailed(err.to_string())),
        }
    }

    /// A vault has just been opened: load what depends on it.
    fn opened(&mut self, mut vault: Vault, key_file: Option<PathBuf>) {
        if let Some(state) = &self.state {
            state.set_last_vault(&self.path);
        }
        let collapsed = vault.local_state().get(KEY_COLLAPSED).ok().flatten().unwrap_or_default();
        self.collapsed = collapsed.split(',').filter_map(|id| id.parse().ok()).collect();

        let settings = read_settings(&mut vault).unwrap_or_default();
        self.auto_lock = match settings.get(&device_key(&vault, "autolock")) {
            Some(text) => minutes(text),
            None => Some(Duration::from_secs(DEFAULT_AUTO_LOCK_MINUTES * 60)),
        };
        #[cfg(feature = "bench")]
        if let Some(limit) = crate::bench::auto_lock() {
            self.auto_lock = Some(limit);
        }
        let unencrypted = vault.kind() == VaultKind::Plain;
        if unencrypted {
            self.auto_lock = None;
        }
        self.sync = parse_sync(&vault, &settings);
        self.active = Instant::now();
        self.vault = Some(vault);
        self.key_file = if unencrypted { None } else { key_file };
        self.asked_remote_key = false;
        self.schedule_interval();
        let key_file = self.key_file.clone();
        self.send(VaultEvent::Phase(Phase::Unlocked { unencrypted, key_file }));
        self.announce_vaults();
        self.send_tree();
        self.announce_sync();
    }

    fn announce_sync(&self) {
        if self.sync.is_none() {
            self.send(VaultEvent::Sync(SyncState::Off));
        }
    }

    /// The database changed: show it, and sync if that is wanted.
    fn changed(&mut self) {
        self.send_tree();
        if self.sync.as_ref().is_some_and(|sync| sync.on_save) {
            self.sync_due = Some(Instant::now() + SYNC_AFTER_SAVE);
        }
    }

    fn send_tree(&mut self) {
        match self.build_tree() {
            Ok(update) => self.send(VaultEvent::Tree(Box::new(update))),
            Err(err) => self.notice(err, true),
        }
    }

    fn build_tree(&mut self) -> Result<TreeUpdate, String> {
        let collapsed = self.collapsed.clone();
        let vault = self.open()?;
        let folders = vault.list::<Folder>().map_err(text)?;
        let connections = vault.list::<Connection>().map_err(text)?;
        let mut rows = vault.list::<Snippet>().map_err(text)?;
        rows.sort_by_cached_key(|row| row.name.to_lowercase());
        let (nodes, mut ids, info) = tree_nodes(folders, connections, &collapsed);
        ids.extend(rows.iter().map(|row| (node_number(row.id), row.id)));
        self.ids = ids;
        let snippets = rows
            .into_iter()
            .map(|row| SnippetRow { id: node_number(row.id), name: row.name, body: row.body })
            .collect();
        Ok(TreeUpdate { model: TreeModel::new(nodes), nodes: info, snippets })
    }

    /// Change the snippet numbered `id`, or add one when there is none.
    fn save_snippet(&mut self, id: Option<u64>, name: String, body: String) -> Result<(), String> {
        let existing = id.map(|id| self.id_of(id)).transpose()?;
        let vault = self.open()?;
        let row = match existing {
            Some(id) => {
                let row = vault
                    .get::<Snippet>(id)
                    .map_err(text)?
                    .ok_or("that snippet no longer exists")?;
                Snippet { name, body, ..row }
            },
            None => Snippet { name, body, ..Default::default() },
        };
        vault.put(&row).map_err(text)
    }

    fn delete_snippet(&mut self, id: u64) -> Result<(), String> {
        let id = self.id_of(id)?;
        self.open()?.delete::<Snippet>(id).map(|_| ()).map_err(text)
    }

    fn folder_toggled(&mut self, node: u64, expanded: bool) {
        let Some(&id) = self.ids.get(&node) else { return };
        if expanded {
            self.collapsed.remove(&id);
        } else {
            self.collapsed.insert(id);
        }
        let joined: Vec<String> = self.collapsed.iter().map(Id::to_string).collect();
        if let Some(vault) = &self.vault
            && let Err(err) = vault.local_state().set(KEY_COLLAPSED, &joined.join(","))
        {
            log::warn!("Could not save the folded folders: {err}");
        }
    }

    fn known_hosts(&mut self, host: &str, port: u16) -> Result<Vec<KnownHost>, String> {
        let rows = self.open()?.list::<KnownHost>().map_err(text)?;
        Ok(rows.into_iter().filter(|row| row.host == host && row.port == port).collect())
    }

    /// Turn a row of the tree into everything connecting to it takes.
    fn plan(&mut self, node: u64) -> Result<ConnectPlan, String> {
        let id = self.id_of(node)?;
        let vault = self.open()?;
        let connection = vault.get::<Connection>(id).map_err(text)?.ok_or("no such connection")?;
        let protocol = parse_protocol(&connection.protocol);

        let mut holds = Vec::new();
        let target = match protocol {
            Protocol::Ssh | Protocol::Sftp => Some(host_config(vault, &connection, &mut holds)?),
            _ => None,
        };
        let credential = match connection.credential_id {
            Some(credential) => vault.get::<Credential>(credential).map_err(text)?,
            None => None,
        };
        let settings = connection.options.as_deref().map(options::parse).unwrap_or_default();
        let endpoint = Endpoint {
            host: connection.host.clone(),
            port: connection.port.or(protocol.default_port()).unwrap_or(0),
            user: connection
                .username
                .clone()
                .or_else(|| credential.as_ref().and_then(|c| c.username.clone()))
                .unwrap_or_default(),
            password: credential.as_ref().map(|c| SecretString::new(c.secret.expose().to_owned())),
            domain: settings.get(OPTION_DOMAIN).cloned(),
        };

        let mut jumps = Vec::new();
        let mut seen = HashSet::from([connection.id]);
        let mut next = connection.jump_connection_id;
        while let Some(jump_id) = next {
            if !seen.insert(jump_id) {
                return Err("the jump hosts of this connection form a loop".to_owned());
            }
            let jump = vault
                .get::<Connection>(jump_id)
                .map_err(text)?
                .ok_or("its jump host no longer exists")?;
            jumps.push(host_config(vault, &jump, &mut holds)?);
            next = jump.jump_connection_id;
        }
        // Each connection names the hop before it, so the walk ends at the first to dial.
        jumps.reverse();

        let tunnels = vault
            .list::<Tunnel>()
            .map_err(text)?
            .into_iter()
            .filter(|tunnel| tunnel.connection_id == connection.id)
            .collect();
        let scale = if settings.get(OPTION_SCALE).is_some_and(|value| value == "native") {
            ScaleChoice::Native
        } else {
            ScaleChoice::Fit
        };
        let rdp_security = match settings.get(OPTION_SECURITY).map(String::as_str) {
            Some("nla") => RdpSecurity::Nla,
            Some("tls") => RdpSecurity::Tls,
            _ => RdpSecurity::Negotiate,
        };
        Ok(ConnectPlan {
            protocol,
            endpoint,
            target,
            jumps,
            tunnels,
            holds,
            scale,
            follow_size: settings.get(OPTION_RESIZE).is_none_or(|value| value != "keep"),
            rdp_security,
            rdp_quality: rdp_quality(&settings),
            rdp: crate::rdp_options::load(&settings),
            vnc_colors: vnc_colors(&settings),
            rustdesk_server: settings.get(OPTION_RENDEZVOUS).cloned(),
            rustdesk_key: settings.get(OPTION_SERVER_KEY).cloned(),
            ftps: settings.get(OPTION_FTPS).is_some_and(|value| value == "1"),
            serial: serial_config(&settings),
        })
    }

    fn folder_for_group(&mut self, group: &str) -> Result<Option<Id>, String> {
        let vault = self.open()?;
        let mut folders = vault.list::<Folder>().map_err(text)?;
        let mut parent = None;
        for name in group.split('/').map(str::trim).filter(|name| !name.is_empty()) {
            let found =
                folders.iter().find(|f| f.name == name && f.parent_id == parent).map(|f| f.id);
            parent = Some(match found {
                Some(id) => id,
                None => {
                    let folder = Folder {
                        parent_id: parent,
                        name: name.to_owned(),
                        sort_order: 0,
                        ..Default::default()
                    };
                    vault.put(&folder).map_err(text)?;
                    let id = folder.id;
                    folders.push(folder);
                    id
                },
            });
        }
        Ok(parent)
    }

    fn save_connection(&mut self, draft: &ConnectionDraft) -> Result<Id, String> {
        let existing = match draft.editing {
            Some(NodeId(node)) => {
                let id = self.id_of(node)?;
                let row = self.open()?.get::<Connection>(id).map_err(text)?;
                Some(row.ok_or("that connection no longer exists")?)
            },
            None => None,
        };
        let forwards = options::parse_forwards(&draft.forwards)?;
        let folder_id = self.folder_for_group(&draft.group)?;

        let vault = self.open()?;
        let mut connection = existing.clone().unwrap_or_default();
        connection.folder_id = folder_id;
        connection.protocol = protocol_name(draft.protocol);
        connection.host = if draft.protocol == Protocol::Serial {
            draft.device.clone()
        } else {
            draft.host.trim().to_owned()
        };
        // A serial port has no port number: its speed does not even fit in one, and is an option.
        connection.port =
            if draft.protocol == Protocol::Serial { None } else { draft.port.trim().parse().ok() };
        connection.name = non_empty(&draft.name).unwrap_or_else(|| connection.host.clone());
        connection.username = non_empty(&draft.username);

        connection.jump_connection_id = match non_empty(&draft.jump) {
            None => None,
            Some(name) => {
                let found = vault
                    .list::<Connection>()
                    .map_err(text)?
                    .into_iter()
                    .find(|c| c.id != connection.id && c.name == name)
                    .ok_or_else(|| format!("There is no saved connection named {name}"))?;
                Some(found.id)
            },
        };

        let mut settings = connection.options.as_deref().map(options::parse).unwrap_or_default();
        settings.remove("auth");
        match draft.auth {
            AuthKind::Password => connection.ssh_key_id = None,
            AuthKind::Key => {
                let path = draft.key_path.trim();
                if !path.is_empty() && !path.starts_with(options::STORED_KEY) {
                    connection.ssh_key_id =
                        Some(ingest_key(vault, &expand_home(path), connection.ssh_key_id)?);
                }
                connection.credential_id = None;
            },
            AuthKind::Agent => {
                settings.insert("auth".to_owned(), "agent".to_owned());
                connection.ssh_key_id = None;
                connection.credential_id = None;
            },
        }
        for key in [
            OPTION_SCALE,
            OPTION_RESIZE,
            OPTION_SECURITY,
            OPTION_QUALITY,
            OPTION_COLORS,
            OPTION_FTPS,
            OPTION_DOMAIN,
            OPTION_BAUD,
            OPTION_DATA_BITS,
            OPTION_PARITY,
            OPTION_STOP_BITS,
            OPTION_FLOW,
            OPTION_RENDEZVOUS,
            OPTION_SERVER_KEY,
        ] {
            settings.remove(key);
        }
        if matches!(draft.protocol, Protocol::Vnc | Protocol::Rdp | Protocol::RustDesk) {
            if draft.scale == ScaleChoice::Native {
                settings.insert(OPTION_SCALE.to_owned(), "native".to_owned());
            }
            if !draft.follow_size {
                settings.insert(OPTION_RESIZE.to_owned(), "keep".to_owned());
            }
        }
        if draft.protocol == Protocol::Rdp {
            match draft.rdp_security {
                RdpSecurity::Negotiate => (),
                RdpSecurity::Nla => {
                    settings.insert(OPTION_SECURITY.to_owned(), "nla".to_owned());
                },
                RdpSecurity::Tls => {
                    settings.insert(OPTION_SECURITY.to_owned(), "tls".to_owned());
                },
            }
            if let Some(domain) = non_empty(&draft.domain) {
                settings.insert(OPTION_DOMAIN.to_owned(), domain);
            }
            if let Some(quality) = rdp_quality_option(draft.rdp_quality) {
                settings.insert(OPTION_QUALITY.to_owned(), quality.to_owned());
            }
            crate::rdp_options::save(&draft.rdp, draft.rdp_quality, &mut settings);
        }
        if draft.protocol == Protocol::RustDesk {
            if let Some(quality) = rdp_quality_option(draft.rdp_quality) {
                settings.insert(OPTION_QUALITY.to_owned(), quality.to_owned());
            }
            if let Some(server) = non_empty(&draft.rustdesk_server) {
                settings.insert(OPTION_RENDEZVOUS.to_owned(), server);
            }
            if let Some(key) = non_empty(&draft.rustdesk_key) {
                settings.insert(OPTION_SERVER_KEY.to_owned(), key);
            }
        }
        if draft.protocol == Protocol::Vnc
            && let Some(colors) = vnc_colors_option(draft.vnc_colors)
        {
            settings.insert(OPTION_COLORS.to_owned(), colors.to_owned());
        }
        if draft.protocol == Protocol::Ftp && draft.ftps {
            settings.insert(OPTION_FTPS.to_owned(), "1".to_owned());
        }
        if draft.protocol == Protocol::Serial {
            settings.insert(OPTION_BAUD.to_owned(), draft.baud.trim().to_owned());
            settings.insert(OPTION_DATA_BITS.to_owned(), draft.data_bits.to_string());
            settings.insert(OPTION_PARITY.to_owned(), draft.parity.to_string());
            settings.insert(OPTION_STOP_BITS.to_owned(), draft.stop_bits.to_string());
            settings.insert(OPTION_FLOW.to_owned(), draft.flow_control.to_string());
        }
        connection.options = (!settings.is_empty()).then(|| options::encode(&settings));

        if !draft.password.is_empty() && draft.auth == AuthKind::Password {
            let credential = Credential {
                id: connection.credential_id.unwrap_or_default(),
                name: connection.name.clone(),
                username: connection.username.clone(),
                secret: Secret::new(draft.password.clone()),
                notes: None,
            };
            connection.credential_id = Some(credential.id);
            vault.put(&credential).map_err(text)?;
        }
        vault.put(&connection).map_err(text)?;
        replace_tunnels(vault, connection.id, forwards)?;
        Ok(connection.id)
    }

    /// The form for a saved connection. Passwords stay in the vault: left empty means unchanged.
    fn draft_of(&mut self, node: u64) -> Result<ConnectionDraft, String> {
        let id = self.id_of(node)?;
        let vault = self.open()?;
        let row = vault.get::<Connection>(id).map_err(text)?.ok_or("no such connection")?;
        let folders = vault.list::<Folder>().map_err(text)?;
        let settings = row.options.as_deref().map(options::parse).unwrap_or_default();

        let mut draft = ConnectionDraft::new(parse_protocol(&row.protocol));
        draft.editing = Some(NodeId(node));
        draft.name = row.name.clone();
        draft.group = folder_path(&folders, row.folder_id);
        if draft.protocol == Protocol::Serial {
            draft.device = row.host.clone();
            if let Some(baud) = settings.get(OPTION_BAUD) {
                draft.baud.clone_from(baud);
            }
            let choice = |key: &str, default: usize| {
                settings.get(key).and_then(|value| value.parse().ok()).unwrap_or(default)
            };
            draft.data_bits = choice(OPTION_DATA_BITS, 3);
            draft.parity = choice(OPTION_PARITY, 0);
            draft.stop_bits = choice(OPTION_STOP_BITS, 0);
            draft.flow_control = choice(OPTION_FLOW, 0);
        } else {
            draft.host = row.host.clone();
            draft.port = row.port.map_or(draft.port, |p| p.to_string());
        }
        draft.username = row.username.clone().unwrap_or_default();
        draft.auth = if row.ssh_key_id.is_some() {
            AuthKind::Key
        } else if settings.get("auth").is_some_and(|auth| auth == "agent") {
            AuthKind::Agent
        } else {
            AuthKind::Password
        };
        if let Some(key_id) = row.ssh_key_id
            && let Some(key) = vault.get::<SshKey>(key_id).map_err(text)?
        {
            draft.key_path = format!("{}{}", options::STORED_KEY, key.name);
        }
        if let Some(jump) = row.jump_connection_id
            && let Some(jump) = vault.get::<Connection>(jump).map_err(text)?
        {
            draft.jump = jump.name;
        }
        let tunnels = vault.list::<Tunnel>().map_err(text)?;
        let specs: Vec<String> = tunnels
            .iter()
            .filter(|t| t.connection_id == row.id)
            .map(options::forward_text)
            .collect();
        draft.forwards = specs.join(", ");
        draft.scale = if settings.get(OPTION_SCALE).is_some_and(|value| value == "native") {
            ScaleChoice::Native
        } else {
            ScaleChoice::Fit
        };
        draft.follow_size = settings.get(OPTION_RESIZE).is_none_or(|value| value != "keep");
        draft.rdp_security = match settings.get(OPTION_SECURITY).map(String::as_str) {
            Some("nla") => RdpSecurity::Nla,
            Some("tls") => RdpSecurity::Tls,
            _ => RdpSecurity::Negotiate,
        };
        draft.rdp_quality = rdp_quality(&settings);
        draft.rdp = crate::rdp_options::load(&settings);
        draft.vnc_colors = vnc_colors(&settings);
        draft.ftps = settings.get(OPTION_FTPS).is_some_and(|value| value == "1");
        draft.domain = settings.get(OPTION_DOMAIN).cloned().unwrap_or_default();
        draft.rustdesk_server = settings.get(OPTION_RENDEZVOUS).cloned().unwrap_or_default();
        draft.rustdesk_key = settings.get(OPTION_SERVER_KEY).cloned().unwrap_or_default();
        Ok(draft)
    }

    fn duplicate(&mut self, node: u64) -> Result<(), String> {
        let id = self.id_of(node)?;
        let vault = self.open()?;
        let row = vault.get::<Connection>(id).map_err(text)?.ok_or("no such connection")?;

        let mut copy = row.clone();
        copy.id = Id::new();
        copy.name = format!("{} (copy)", row.name);
        // A row of its own, so that changing the copy's password leaves the original's alone.
        if let Some(credential) = row.credential_id
            && let Some(credential) = vault.get::<Credential>(credential).map_err(text)?
        {
            let credential = Credential { id: Id::new(), ..credential };
            copy.credential_id = Some(credential.id);
            vault.put(&credential).map_err(text)?;
        }
        vault.put(&copy).map_err(text)?;
        for tunnel in vault.list::<Tunnel>().map_err(text)? {
            if tunnel.connection_id == row.id {
                let tunnel = Tunnel { id: Id::new(), connection_id: copy.id, ..tunnel };
                vault.put(&tunnel).map_err(text)?;
            }
        }
        Ok(())
    }

    fn delete_connection(&mut self, node: u64) -> Result<(), String> {
        let id = self.id_of(node)?;
        let vault = self.open()?;
        let row = vault.get::<Connection>(id).map_err(text)?.ok_or("no such connection")?;
        vault.delete::<Connection>(id).map_err(text)?;
        for tunnel in vault.list::<Tunnel>().map_err(text)? {
            if tunnel.connection_id == id {
                vault.delete::<Tunnel>(tunnel.id).map_err(text)?;
            }
        }
        let others = vault.list::<Connection>().map_err(text)?;
        if let Some(credential) = row.credential_id
            && !others.iter().any(|c| c.credential_id == Some(credential))
        {
            vault.delete::<Credential>(credential).map_err(text)?;
        }
        if let Some(key) = row.ssh_key_id
            && !others.iter().any(|c| c.ssh_key_id == Some(key))
        {
            vault.delete::<SshKey>(key).map_err(text)?;
        }
        Ok(())
    }

    fn import_ssh_config(&mut self) -> Result<String, String> {
        let home = std::env::home_dir().ok_or("There is no home directory")?;
        let path = home.join(".ssh/config");
        let config = rmx_ssh::ssh_config::SshConfig::load(&path)
            .map_err(|err| format!("Could not read {}: {err}", path.display()))?;
        let imported = import_hosts(self.open()?, &config)?;
        let noun = if imported == 1 { "host" } else { "hosts" };
        Ok(format!("Imported {imported} {noun} from ~/.ssh/config"))
    }

    fn load_settings(&mut self, theme: ThemeMode) -> Result<SettingsDraft, String> {
        let vault = self.open()?;
        let map = read_settings(vault)?;
        let auto_lock = map.get(&device_key(vault, "autolock")).cloned();
        let sync = parse_sync(vault, &map);
        Ok(SettingsDraft {
            theme,
            auto_lock_minutes: auto_lock.unwrap_or_else(|| DEFAULT_AUTO_LOCK_MINUTES.to_string()),
            sync_on_save: sync.as_ref().is_none_or(|sync| sync.on_save),
            sync_interval_minutes: sync
                .as_ref()
                .and_then(|sync| sync.interval)
                .map(|every| (every.as_secs() / 60).to_string())
                .unwrap_or_default(),
            sync: sync.map(|sync| sync.draft).unwrap_or_default(),
        })
    }

    fn save_settings(&mut self, draft: &SettingsDraft) -> Result<(), String> {
        let vault = self.open()?;
        let settings = SyncSettings {
            draft: draft.sync.clone(),
            on_save: draft.sync_on_save,
            interval: minutes(&draft.sync_interval_minutes),
        };
        store_sync_settings(vault, &settings, &draft.auto_lock_minutes)?;
        self.auto_lock =
            minutes(&draft.auto_lock_minutes).filter(|_| vault.kind() != VaultKind::Plain);
        self.sync = (settings.draft.kind != SyncKind::None).then_some(settings);
        self.active = Instant::now();
        self.schedule_interval();
        self.announce_sync();
        Ok(())
    }

    /// Build the backend `draft` describes. Connecting to an SFTP server takes place here.
    fn backend(
        &self,
        draft: &SyncDraft,
        known: Vec<KnownHost>,
        interactive: bool,
    ) -> Result<(Box<dyn SyncBackend>, TrustedKeys), String> {
        let trusted = TrustedKeys::default();
        let backend: Box<dyn SyncBackend> = match draft.kind {
            SyncKind::None => return Err("sync is off".to_owned()),
            SyncKind::Local => {
                let path = local_sync_file(&expand_home(draft.path.trim()));
                Box::new(LocalPathBackend::new(path))
            },
            SyncKind::WebDav => Box::new(
                WebDavBackend::new(WebDavConfig {
                    url: draft.url.trim().to_owned(),
                    username: non_empty(&draft.user),
                    password: Secret::new(draft.secret.clone()),
                    allow_plain_http: false,
                })
                .map_err(text)?,
            ),
            SyncKind::S3 => Box::new(
                S3Backend::new(S3Config {
                    endpoint: draft.url.trim().to_owned(),
                    bucket: draft.bucket.trim().to_owned(),
                    region: draft.region.trim().to_owned(),
                    key: draft.key.trim().to_owned(),
                    access_key: draft.access_key.trim().to_owned(),
                    secret_key: Secret::new(draft.secret.clone()),
                    path_style: true,
                    allow_plain_http: false,
                })
                .map_err(text)?,
            ),
            SyncKind::Sftp => {
                let port = draft.port.trim().parse().unwrap_or(22);
                let (verifier, keys) = sync_verifier(self.proxy.clone(), known, interactive);
                let target = SftpTarget {
                    host: draft.host.trim().to_owned(),
                    port,
                    user: draft.user.trim().to_owned(),
                    password: SecretString::new(draft.secret.clone()),
                };
                let label = format!("{}@{}:{port}", target.user, target.host);
                let files =
                    SftpFiles::connect(self.runtime.clone(), target, verifier).map_err(text)?;
                let backend = SftpBackend::new(files, label, draft.path.trim().to_owned());
                return Ok((Box::new(backend), keys));
            },
        };
        Ok((backend, trusted))
    }

    fn run_sync(&mut self, interactive: bool) {
        if self.sync.is_none() {
            self.send(VaultEvent::Sync(SyncState::Off));
            return;
        }
        if !self.is_open() {
            return;
        }
        match self.with_backend(interactive, |vault, backend| vault.sync(backend)) {
            Ok(Err(Error::RemoteKeyChanged)) => {
                self.send(VaultEvent::Sync(SyncState::Failed(describe_sync_error(
                    Error::RemoteKeyChanged,
                ))));
                if !std::mem::replace(&mut self.asked_remote_key, true) {
                    self.send(VaultEvent::RemoteKeyChanged);
                }
            },
            Ok(result) => self.report_sync(result.map_err(describe_sync_error)),
            Err(err) => self.report_sync(Err(err)),
        }
    }

    /// Say how a sync went, and show what it brought.
    fn report_sync(&mut self, result: Result<SyncReport, String>) {
        match result {
            Ok(report) => {
                self.send(VaultEvent::Sync(SyncState::Done(describe_report(&report))));
                if report.changed_locally {
                    self.send_tree();
                }
            },
            Err(err) => self.send(VaultEvent::Sync(SyncState::Failed(err))),
        }
    }

    /// Run `work` against the open vault and its sync backend, keeping the host keys the user
    /// trusted on the way. The outer error is a backend that could not be got going.
    fn with_backend<T>(
        &mut self,
        interactive: bool,
        work: impl FnOnce(&mut Vault, &dyn SyncBackend) -> rmx_vault::Result<T>,
    ) -> Result<rmx_vault::Result<T>, String> {
        let settings = self.sync.clone().ok_or("sync is off")?;
        off_main("sync");
        self.send(VaultEvent::Sync(SyncState::Running));
        let known = self.open()?.list::<KnownHost>().map_err(text)?;
        let (backend, trusted) = self.backend(&settings.draft, known, interactive)?;
        let vault = self.open()?;
        let result = work(vault, backend.as_ref());
        drop(backend);

        let rows = trusted.lock().map(|mut rows| std::mem::take(&mut *rows)).unwrap_or_default();
        for row in rows {
            let _ = vault.put(&row);
        }
        Ok(result)
    }

    /// Re-encrypt with `new` and `key_file`, once `current` has been shown to be the password.
    /// With sync set up the synced copy goes over to it as well, or else nothing changes.
    fn change_password(
        &mut self,
        current: Secret,
        new: Secret,
        key_file: Option<PathBuf>,
    ) -> Result<String, String> {
        if new.is_empty() && key_file.is_none() {
            return Err("Enter a new master password or a key file".to_owned());
        }
        off_main("argon2");
        let old_key_file = self.key_file.clone();
        let old = Self::credentials(current, old_key_file.clone());
        let new = Self::credentials(new, key_file.clone());
        let vault = self.open()?;
        vault.check_credentials(&old).map_err(|err| match err {
            Error::WrongKey => "The current master password is wrong".to_owned(),
            Error::Unencrypted => "This vault has no master password to change".to_owned(),
            other => other.to_string(),
        })?;
        vault.change_credentials(&new, KdfParams::RECOMMENDED).map_err(text)?;
        self.key_file = key_file;
        if self.sync.is_none() {
            return Ok("Master password changed".to_owned());
        }
        let pushed = self
            .with_backend(true, |vault, backend| vault.push_new_credentials(backend, &old))
            .and_then(|result| result.map_err(describe_sync_error));
        match pushed {
            Ok(report) => {
                self.report_sync(Ok(report));
                Ok("Master password changed, here and in the synced copy".to_owned())
            },
            Err(err) => {
                // An error after the upload leaves the synced copy on the new password already:
                // putting the old one back here would split the two.
                let landed = self
                    .with_backend(false, |vault, backend| vault.remote_has_current_key(backend));
                if matches!(landed, Ok(Ok(true))) {
                    self.run_sync(false);
                    return Ok("Master password changed, here and in the synced copy".to_owned());
                }
                self.report_sync(Err(err.clone()));
                let undone = self.open().and_then(|vault| {
                    vault.change_credentials(&old, KdfParams::RECOMMENDED).map_err(text)
                });
                match undone {
                    Ok(()) => {
                        self.key_file = old_key_file;
                        Err(format!(
                            "The synced copy could not be updated, so the master password is \
                             unchanged: {err}"
                        ))
                    },
                    Err(back) => Err(format!(
                        "The synced copy could not be updated ({err}) and the old master password \
                         could not be put back ({back}): this vault now opens with the new one"
                    )),
                }
            },
        }
    }

    /// Follow a password change made on another device.
    fn adopt_password(
        &mut self,
        password: Secret,
        key_file: Option<PathBuf>,
    ) -> Result<String, String> {
        if password.is_empty() && key_file.is_none() {
            return Err("Enter the new master password or key file".to_owned());
        }
        off_main("argon2");
        let credentials = Self::credentials(password, key_file.clone());
        let adopted = self
            .with_backend(true, |vault, backend| {
                vault.adopt_remote_credentials(backend, &credentials)
            })
            .and_then(|result| result.map_err(describe_sync_error));
        match adopted {
            Ok(report) => {
                self.key_file = key_file;
                self.asked_remote_key = false;
                self.report_sync(Ok(report));
                Ok("This vault now uses the new master password".to_owned())
            },
            Err(err) => {
                self.report_sync(Err(err.clone()));
                Err(err)
            },
        }
    }
}

type TrustedKeys = std::sync::Arc<std::sync::Mutex<Vec<KnownHost>>>;

/// What the form saves for an RDP quality; nothing for the default.
fn rdp_quality_option(quality: RdpQuality) -> Option<&'static str> {
    match quality {
        RdpQuality::Best => Some("best"),
        RdpQuality::Balanced => None,
        RdpQuality::Fast => Some("fast"),
        RdpQuality::Custom => Some("custom"),
    }
}

/// What the form saves for a VNC color depth; nothing for the default.
fn vnc_colors_option(colors: VncColors) -> Option<&'static str> {
    match colors {
        VncColors::Full => None,
        VncColors::Bits16 => Some("16"),
        VncColors::Bits8 => Some("8"),
    }
}

/// How much picture an RDP connection trades for bandwidth, from what the form saved.
fn rdp_quality(settings: &BTreeMap<String, String>) -> RdpQuality {
    match settings.get(OPTION_QUALITY).map(String::as_str) {
        Some("best") => RdpQuality::Best,
        Some("fast") => RdpQuality::Fast,
        Some("custom") => RdpQuality::Custom,
        _ => RdpQuality::Balanced,
    }
}

/// How many colors a VNC connection asks for, from what the form saved.
fn vnc_colors(settings: &BTreeMap<String, String>) -> VncColors {
    match settings.get(OPTION_COLORS).map(String::as_str) {
        Some("16") => VncColors::Bits16,
        Some("8") => VncColors::Bits8,
        _ => VncColors::Full,
    }
}

/// How to set up a serial port, from what the form saved.
fn serial_config(settings: &BTreeMap<String, String>) -> SerialConfig {
    let choice = |key: &str| settings.get(key).and_then(|value| value.parse::<usize>().ok());
    let defaults = SerialConfig::default();
    SerialConfig {
        baud_rate: settings
            .get(OPTION_BAUD)
            .and_then(|baud| baud.parse().ok())
            .unwrap_or(defaults.baud_rate),
        data_bits: match choice(OPTION_DATA_BITS) {
            Some(0) => DataBits::Five,
            Some(1) => DataBits::Six,
            Some(2) => DataBits::Seven,
            _ => DataBits::Eight,
        },
        parity: match choice(OPTION_PARITY) {
            Some(1) => Parity::Odd,
            Some(2) => Parity::Even,
            _ => Parity::None,
        },
        stop_bits: if choice(OPTION_STOP_BITS) == Some(1) { StopBits::Two } else { StopBits::One },
        flow_control: match choice(OPTION_FLOW) {
            Some(1) => FlowControl::Software,
            Some(2) => FlowControl::Hardware,
            _ => FlowControl::None,
        },
    }
}

fn text(err: impl std::fmt::Display) -> String {
    err.to_string()
}

fn describe_sync_error(err: Error) -> String {
    match err {
        Error::RemoteKeyChanged => "The synced copy has a different master password".to_owned(),
        Error::WrongKey => "That is not the synced copy's master password or key file".to_owned(),
        other => other.to_string(),
    }
}

fn clock_now() -> String {
    let now =
        SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map_or(0, |now| now.as_secs());
    rmx_ui::local_time(now)
        .map_or_else(String::new, |local| format!("{:02}:{:02}", local.hour, local.minute))
}

fn describe_report(report: &SyncReport) -> String {
    let what = match (report.changed_locally, report.pushed) {
        (true, true) => "merged and pushed",
        (true, false) => "merged",
        (false, true) => "pushed",
        (false, false) => "up to date",
    };
    format!("Synced at {}, {what}", clock_now())
}

/// Rename, or copy and remove where the two are on different filesystems.
fn move_file(from: &Path, to: &Path) -> std::io::Result<()> {
    if std::fs::rename(from, to).is_ok() {
        return Ok(());
    }
    std::fs::copy(from, to)?;
    std::fs::remove_file(from)
}

/// Move a closed vault's file, and the files beside it, making the folder it goes to.
fn move_vault(from: &Path, to: &Path) -> Result<(), String> {
    if let Some(parent) = to.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|err| format!("Could not make {}: {err}", parent.display()))?;
    }
    move_file(from, to).map_err(|err| format!("Could not move the vault: {err}"))?;
    // Its own settings for this device, and whatever a crash left of the journal.
    for suffix in [".local", "-wal", "-shm", "-journal"] {
        let beside = |path: &Path| {
            let mut name = path.as_os_str().to_owned();
            name.push(suffix);
            PathBuf::from(name)
        };
        let (from, to) = (beside(from), beside(to));
        if from.exists()
            && let Err(err) = move_file(&from, &to)
        {
            log::warn!("Could not move {}: {err}", from.display());
        }
    }
    Ok(())
}

/// A folder names the file in it.
fn local_sync_file(path: &Path) -> PathBuf {
    if path.is_dir() { path.join("rmx.rmxdb") } else { path.to_owned() }
}

/// Rows of the device's own settings, whose name carries this device's id.
fn device_key(vault: &Vault, name: &str) -> String {
    let node = vault.local_state().node_id().unwrap_or(0);
    format!("device.{node:016x}.{name}")
}

fn read_settings(vault: &mut Vault) -> Result<HashMap<String, String>, String> {
    let rows = vault.list::<SharedSetting>().map_err(text)?;
    Ok(rows.into_iter().map(|row| (row.key, row.value)).collect())
}

fn parse_sync(vault: &Vault, map: &HashMap<String, String>) -> Option<SyncSettings> {
    let get = |name: &str| map.get(&device_key(vault, name)).cloned().unwrap_or_default();
    let kind = match get("sync.kind").as_str() {
        "local" => SyncKind::Local,
        "webdav" => SyncKind::WebDav,
        "s3" => SyncKind::S3,
        "sftp" => SyncKind::Sftp,
        _ => return None,
    };
    let draft = SyncDraft {
        kind,
        path: get("sync.path"),
        url: get("sync.url"),
        user: get("sync.user"),
        secret: get("sync.secret"),
        bucket: get("sync.bucket"),
        region: get("sync.region"),
        key: get("sync.key"),
        access_key: get("sync.access_key"),
        host: get("sync.host"),
        port: get("sync.port"),
    };
    Some(SyncSettings {
        draft,
        on_save: get("sync.on_save") != "0",
        interval: minutes(&get("sync.interval")),
    })
}

fn store_sync_settings(
    vault: &mut Vault,
    settings: &SyncSettings,
    auto_lock: &str,
) -> Result<(), String> {
    let draft = &settings.draft;
    let kind = match draft.kind {
        SyncKind::None => "",
        SyncKind::Local => "local",
        SyncKind::WebDav => "webdav",
        SyncKind::S3 => "s3",
        SyncKind::Sftp => "sftp",
    };
    let interval = settings.interval.map(|every| (every.as_secs() / 60).to_string());
    let on_save = if settings.on_save { "1" } else { "0" };
    let entries = [
        ("sync.kind", kind),
        ("sync.path", draft.path.as_str()),
        ("sync.url", draft.url.as_str()),
        ("sync.user", draft.user.as_str()),
        ("sync.secret", draft.secret.as_str()),
        ("sync.bucket", draft.bucket.as_str()),
        ("sync.region", draft.region.as_str()),
        ("sync.key", draft.key.as_str()),
        ("sync.access_key", draft.access_key.as_str()),
        ("sync.host", draft.host.as_str()),
        ("sync.port", draft.port.as_str()),
        ("sync.on_save", on_save),
        ("sync.interval", interval.as_deref().unwrap_or("")),
        ("autolock", auto_lock),
    ];
    for (name, value) in entries {
        let key = device_key(vault, name);
        vault.put(&SharedSetting::new(&key, value)).map_err(text)?;
    }
    Ok(())
}

/// How the SSH library is to log in to the connection `row`.
fn host_config(
    vault: &mut Vault,
    row: &Connection,
    holds: &mut Vec<KeyHold>,
) -> Result<HostConfig, String> {
    let credential = match row.credential_id {
        Some(id) => vault.get::<Credential>(id).map_err(text)?,
        None => None,
    };
    let user = row
        .username
        .clone()
        .or_else(|| credential.as_ref().and_then(|c| c.username.clone()))
        .or_else(|| std::env::var("USER").ok())
        .unwrap_or_else(|| "root".to_owned());

    let settings = row.options.as_deref().map(options::parse).unwrap_or_default();
    let mut auth = Vec::new();
    if let Some(key_id) = row.ssh_key_id {
        let key = vault.get::<SshKey>(key_id).map_err(text)?.ok_or("its stored key is gone")?;
        let hold = KeyHold::new(&key.private_key)?;
        auth.push(AuthMethod::PrivateKeyFile {
            path: hold.path(),
            passphrase: key.passphrase.map(|p| SecretString::new(p.expose().to_owned())),
        });
        holds.push(hold);
    }
    if let Some(files) = settings.get("identity_files") {
        auth.extend(
            files
                .split('\n')
                .map(PathBuf::from)
                .filter(|path| path.exists())
                .map(|path| AuthMethod::PrivateKeyFile { path, passphrase: None }),
        );
    }
    if settings.get("auth").is_some_and(|auth| auth == "agent")
        || settings.contains_key("identity_files")
    {
        auth.push(AuthMethod::Agent { socket: None });
    }
    if let Some(credential) = &credential {
        let secret = SecretString::new(credential.secret.expose().to_owned());
        auth.push(AuthMethod::Password(Some(secret)));
    }
    auth.push(AuthMethod::KeyboardInteractive { submethods: None });
    auth.push(AuthMethod::Password(None));
    let port = row.port.unwrap_or(22);
    Ok(HostConfig::new(row.host.clone(), user, auth).with_port(port))
}

/// Read a private key file into the vault, in `existing` if there is one. Returns its id.
fn ingest_key(vault: &mut Vault, path: &Path, existing: Option<Id>) -> Result<Id, String> {
    let private = std::fs::read_to_string(path)
        .map_err(|err| format!("Could not read {}: {err}", path.display()))?;
    let public_key = std::fs::read_to_string(path.with_extension("pub")).unwrap_or_default();
    let algorithm = public_key.split_whitespace().next().unwrap_or("unknown").to_owned();
    let name = path.file_name().map_or_else(String::new, |n| n.to_string_lossy().into_owned());
    let row = SshKey {
        id: existing.unwrap_or_default(),
        name,
        algorithm,
        public_key: public_key.trim().to_owned(),
        private_key: Secret::new(private),
        passphrase: None,
    };
    vault.put(&row).map_err(text)?;
    Ok(row.id)
}

fn replace_tunnels(vault: &mut Vault, connection: Id, wanted: Vec<Tunnel>) -> Result<(), String> {
    let wanted: Vec<Tunnel> = wanted
        .into_iter()
        .map(|tunnel| Tunnel {
            id: Id::derived(&["tunnel", &connection.to_string(), &options::forward_text(&tunnel)]),
            connection_id: connection,
            ..tunnel
        })
        .collect();
    for old in vault.list::<Tunnel>().map_err(text)? {
        if old.connection_id == connection && !wanted.iter().any(|tunnel| tunnel.id == old.id) {
            vault.delete::<Tunnel>(old.id).map_err(text)?;
        }
    }
    for tunnel in &wanted {
        vault.put(tunnel).map_err(text)?;
    }
    Ok(())
}

fn folder_path(folders: &[Folder], mut at: Option<Id>) -> String {
    let mut parts = Vec::new();
    while let Some(folder) = at.and_then(|id| folders.iter().find(|f| f.id == id)) {
        parts.push(folder.name.as_str());
        at = folder.parent_id;
        if parts.len() > folders.len() {
            break;
        }
    }
    parts.reverse();
    parts.join("/")
}

/// Write the hosts of `config` into the vault. Doing it twice changes nothing.
fn import_hosts(
    vault: &mut Vault,
    config: &rmx_ssh::ssh_config::SshConfig,
) -> Result<usize, String> {
    let folder = Folder {
        id: Id::derived(&["ssh_config", "folder"]),
        parent_id: None,
        name: IMPORT_FOLDER.to_owned(),
        sort_order: 0,
    };
    vault.put(&folder).map_err(text)?;

    let id_of = |alias: &str| Id::derived(&["ssh_config", alias]);
    let aliases = config.hosts();
    let mut count = 0;
    for alias in &aliases {
        let host = config.resolve(alias);
        let mut settings = BTreeMap::new();
        if !host.identity_files.is_empty() {
            let files: Vec<String> =
                host.identity_files.iter().map(|p| p.display().to_string()).collect();
            settings.insert("identity_files".to_owned(), files.join("\n"));
        }

        // `ProxyJump a,b`: b is reached through a. Hops that are not aliases get rows of their own.
        let mut previous = None;
        for spec in &host.proxy_jump {
            let id = if aliases.contains(&spec.host) {
                id_of(&spec.host)
            } else {
                let row = Connection {
                    id: Id::derived(&["ssh_config", "jump", &spec.host]),
                    folder_id: Some(folder.id),
                    name: spec.host.clone(),
                    protocol: "ssh".to_owned(),
                    host: spec.host.clone(),
                    port: spec.port,
                    username: spec.user.clone(),
                    jump_connection_id: previous,
                    ..Default::default()
                };
                vault.put(&row).map_err(text)?;
                row.id
            };
            previous = Some(id);
        }

        let row = Connection {
            id: id_of(alias),
            folder_id: Some(folder.id),
            name: alias.clone(),
            protocol: "ssh".to_owned(),
            host: host.host_name.clone(),
            port: host.port,
            username: host.user.clone(),
            jump_connection_id: previous,
            options: (!settings.is_empty()).then(|| options::encode(&settings)),
            ..Default::default()
        };
        vault.put(&row).map_err(text)?;
        count += 1;
    }
    Ok(count)
}

type Tree = (Vec<TreeNode>, HashMap<u64, Id>, HashMap<u64, NodeInfo>);

/// Lay out the rows depth first. A parent that is gone, or a cycle that syncing made, is the root.
fn tree_nodes(folders: Vec<Folder>, connections: Vec<Connection>, collapsed: &HashSet<Id>) -> Tree {
    let known: HashSet<Id> = folders.iter().map(|f| f.id).collect();
    let root_of = |parent: Option<Id>| parent.filter(|id| known.contains(id));

    let mut child_folders: HashMap<Option<Id>, Vec<&Folder>> = HashMap::new();
    for folder in &folders {
        child_folders.entry(root_of(folder.parent_id)).or_default().push(folder);
    }
    let mut child_connections: HashMap<Option<Id>, Vec<&Connection>> = HashMap::new();
    for connection in &connections {
        child_connections.entry(root_of(connection.folder_id)).or_default().push(connection);
    }
    for list in child_folders.values_mut() {
        list.sort_by(|a, b| (a.sort_order, &a.name).cmp(&(b.sort_order, &b.name)));
    }
    for list in child_connections.values_mut() {
        list.sort_by(|a, b| (a.sort_order, &a.name).cmp(&(b.sort_order, &b.name)));
    }

    struct Walk<'a> {
        folders: &'a HashMap<Option<Id>, Vec<&'a Folder>>,
        connections: &'a HashMap<Option<Id>, Vec<&'a Connection>>,
        collapsed: &'a HashSet<Id>,
        visited: HashSet<Id>,
        nodes: Vec<TreeNode>,
        ids: HashMap<u64, Id>,
        info: HashMap<u64, NodeInfo>,
    }
    impl Walk<'_> {
        fn folder(&mut self, folder: &Folder, depth: u16) {
            if !self.visited.insert(folder.id) {
                return;
            }
            let id = node_number(folder.id);
            self.ids.insert(id, folder.id);
            self.info.insert(
                id,
                NodeInfo { name: folder.name.clone(), kind: NodeKind::Folder, host: String::new() },
            );
            self.nodes.push(TreeNode {
                id: NodeId(id),
                depth,
                kind: NodeKind::Folder,
                label: folder.name.clone(),
                state: SessionDot::None,
                expanded: !self.collapsed.contains(&folder.id),
            });
            self.level(Some(folder.id), depth + 1);
        }

        fn level(&mut self, parent: Option<Id>, depth: u16) {
            for folder in self.folders.get(&parent).cloned().unwrap_or_default() {
                self.folder(folder, depth);
            }
            for connection in self.connections.get(&parent).cloned().unwrap_or_default() {
                let id = node_number(connection.id);
                let kind = NodeKind::Connection(parse_protocol(&connection.protocol));
                self.ids.insert(id, connection.id);
                self.info.insert(
                    id,
                    NodeInfo { name: connection.name.clone(), kind, host: connection.host.clone() },
                );
                self.nodes.push(TreeNode {
                    id: NodeId(id),
                    depth,
                    kind,
                    label: connection.name.clone(),
                    state: SessionDot::None,
                    expanded: false,
                });
            }
        }
    }

    let mut walk = Walk {
        folders: &child_folders,
        connections: &child_connections,
        collapsed,
        visited: HashSet::new(),
        nodes: Vec::new(),
        ids: HashMap::new(),
        info: HashMap::new(),
    };
    walk.level(None, 0);
    // Whatever a cycle kept out of reach.
    for folder in &folders {
        walk.folder(folder, 0);
    }
    (walk.nodes, walk.ids, walk.info)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn folder(name: &str, parent: Option<Id>) -> Folder {
        Folder { name: name.to_owned(), parent_id: parent, ..Default::default() }
    }

    fn connection(name: &str, folder: Option<Id>) -> Connection {
        Connection { name: name.to_owned(), folder_id: folder, ..Default::default() }
    }

    fn labels(nodes: &[TreeNode]) -> Vec<(u16, &str)> {
        nodes.iter().map(|node| (node.depth, node.label.as_str())).collect()
    }

    #[test]
    fn the_tree_is_depth_first_with_folders_first() {
        let a = folder("a", None);
        let b = folder("b", Some(a.id));
        let conns = vec![
            connection("in-b", Some(b.id)),
            connection("in-a", Some(a.id)),
            connection("top", None),
        ];
        let (nodes, ..) = tree_nodes(vec![b, a], conns, &HashSet::new());
        assert_eq!(labels(&nodes), [(0, "a"), (1, "b"), (2, "in-b"), (1, "in-a"), (0, "top")]);
    }

    #[test]
    fn a_missing_parent_is_the_root_and_a_cycle_still_shows() {
        let orphan = folder("orphan", Some(Id::new()));
        let mut x = folder("x", None);
        let y = folder("y", Some(x.id));
        x.parent_id = Some(y.id);
        let lost = connection("lost", Some(Id::new()));
        let (nodes, ..) = tree_nodes(vec![orphan, x, y], vec![lost], &HashSet::new());
        let found = labels(&nodes);
        assert!(found.contains(&(0, "orphan")));
        assert!(found.contains(&(0, "lost")));
        assert!(found.iter().any(|&(_, label)| label == "x"));
        assert!(found.iter().any(|&(_, label)| label == "y"));
    }

    #[test]
    fn folding_is_remembered_by_folder() {
        let a = folder("a", None);
        let collapsed = HashSet::from([a.id]);
        let (nodes, ..) = tree_nodes(vec![a], Vec::new(), &collapsed);
        assert!(!nodes[0].expanded);
    }

    #[test]
    fn display_quality_reads_back_as_saved() {
        let saved = |key: &str, value: Option<&str>| {
            value
                .map(|value| BTreeMap::from([(key.to_owned(), value.to_owned())]))
                .unwrap_or_default()
        };
        for quality in
            [RdpQuality::Best, RdpQuality::Balanced, RdpQuality::Fast, RdpQuality::Custom]
        {
            assert_eq!(rdp_quality(&saved(OPTION_QUALITY, rdp_quality_option(quality))), quality);
        }
        for colors in [VncColors::Full, VncColors::Bits16, VncColors::Bits8] {
            assert_eq!(vnc_colors(&saved(OPTION_COLORS, vnc_colors_option(colors))), colors);
        }
        // Connections saved before there was a choice keep what they had.
        assert_eq!(rdp_quality(&BTreeMap::new()), RdpQuality::Balanced);
        assert_eq!(vnc_colors(&BTreeMap::new()), VncColors::Full);
    }

    #[test]
    fn numbers_are_stable() {
        let id = Id::new();
        assert_eq!(node_number(id), node_number(id.to_string().parse().unwrap()));
    }

    #[test]
    fn importing_twice_changes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let creds = Credentials::new("test");
        let params = KdfParams::new(5, 1, 1).unwrap();
        let mut vault = Vault::create(&dir.path().join("v.rmxdb"), &creds, params).unwrap();

        let config = rmx_ssh::ssh_config::SshConfig::parse(
            "Host web\n  HostName 10.0.0.1\n  User deploy\n  ProxyJump bastion\nHost bastion\n  \
             HostName 10.0.0.2\n  Port 2200\n",
        );
        assert_eq!(import_hosts(&mut vault, &config).unwrap(), 2);
        let digest = vault.digest().unwrap();
        assert_eq!(import_hosts(&mut vault, &config).unwrap(), 2);
        assert_eq!(vault.digest().unwrap(), digest);

        let rows = vault.list::<Connection>().unwrap();
        let web = rows.iter().find(|row| row.name == "web").unwrap();
        let bastion = rows.iter().find(|row| row.name == "bastion").unwrap();
        assert_eq!(web.jump_connection_id, Some(bastion.id));
        assert_eq!(bastion.port, Some(2200));
    }

    fn plain(password: &str, key_file: Option<&str>, agreed: bool) -> Result<bool, String> {
        let protection =
            Protection::asked_for(Secret::new(password), key_file.map(PathBuf::from), agreed)?;
        Ok(matches!(protection, Protection::Plain))
    }

    #[test]
    fn a_database_with_no_password_takes_the_users_word() {
        assert_eq!(plain("", None, true), Ok(true));
        assert_eq!(plain("", None, false), Err(PLAIN_NOT_AGREED.to_owned()));
    }

    #[test]
    fn anything_to_protect_it_with_is_encryption_whatever_was_agreed() {
        assert_eq!(plain("secret", None, false), Ok(false));
        assert_eq!(plain("secret", None, true), Ok(false));
        assert_eq!(plain("secret", Some("/k"), false), Ok(false));
        assert_eq!(plain("", Some("/k"), true), Ok(false));
    }

    #[test]
    fn a_moved_vault_opens_where_it_went_with_its_device_state() {
        let dir = tempfile::tempdir().unwrap();
        let (from, to) = (dir.path().join("v.rmxdb"), dir.path().join("synced/folder/moved.rmxdb"));
        let vault = Vault::create_plain(&from).unwrap();
        let node = vault.local_state().node_id().unwrap();
        drop(vault);

        move_vault(&from, &to).unwrap();

        assert!(!from.exists() && !dir.path().join("v.rmxdb.local").exists());
        let vault = Vault::open_plain(&to).unwrap();
        assert_eq!(vault.local_state().node_id().unwrap(), node);
    }

    #[test]
    fn a_held_key_reads_back() {
        let hold = KeyHold::new(&Secret::new("private material")).unwrap();
        let path = hold.path();
        #[cfg(target_os = "linux")]
        assert!(path.starts_with("/proc/self/fd"));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "private material");
    }
}
