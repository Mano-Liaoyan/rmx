//! Everything that must stay off the main thread, and the way back to it.
//!
//! Started before the window so that the first frame can use what it finds: the runtime for
//! sessions, and the state file saying how the window looked and which vault was last used.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::thread::{self, JoinHandle, ThreadId};

use rmx_layout::session::{self, SavedWindow};
use rmx_ui::ThemeMode;
use rmx_ui::widgets::Panels;
use rmx_vault::LocalState;
use tokio::runtime::{Builder, Runtime};

/// Worker threads of the runtime: sessions wait on the network, so few are plenty.
const WORKERS: usize = 4;

const KEY_WIDTH: &str = "window.width";
const KEY_HEIGHT: &str = "window.height";
const KEY_MAXIMIZED: &str = "window.maximized";
const KEY_SIDEBAR: &str = "sidebar.open";
const KEY_THEME: &str = "theme";
const KEY_LAST_VAULT: &str = "last_vault";
const KEY_RECENT_VAULTS: &str = "recent_vaults";
/// How many vaults the switcher remembers.
const RECENT_VAULTS: usize = 12;
const KEY_SESSION: &str = "session";
const KEY_PANELS: &str = "panels";

static MAIN_THREAD: OnceLock<ThreadId> = OnceLock::new();

/// The calling thread is the main one.
pub fn mark_main_thread() {
    let _ = MAIN_THREAD.set(thread::current().id());
}

/// Slow work called `what` is about to run, which must not be on the main thread.
pub fn off_main(what: &'static str) {
    let on_main = MAIN_THREAD.get() == Some(&thread::current().id());
    debug_assert!(!on_main, "{what} ran on the main thread");
    #[cfg(feature = "bench")]
    crate::bench::off_main(what, on_main);
    #[cfg(not(feature = "bench"))]
    let _ = what;
}

/// What a pane of a saved session was. Numbers only: the state file is not encrypted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SavedPane {
    Local,
    /// The saved connection numbered this, for what it is for.
    Node(u64),
    /// Browsing the files of the saved connection numbered this.
    Files(u64),
}

impl SavedPane {
    /// How it goes in the state file.
    pub fn spell(self) -> String {
        match self {
            Self::Local => "local".to_owned(),
            Self::Node(node) => format!("node {node}"),
            Self::Files(node) => format!("files {node}"),
        }
    }

    fn read(text: &str) -> Option<Self> {
        if text == "local" {
            return Some(Self::Local);
        }
        let (kind, node) = text.split_once(' ')?;
        let node = node.parse().ok()?;
        match kind {
            "node" => Some(Self::Node(node)),
            "files" => Some(Self::Files(node)),
            _ => None,
        }
    }
}

/// How the windows were left, none when there is nothing worth reopening.
pub type SavedSession = Vec<SavedWindow<SavedPane>>;

/// Read a session, as the state file has it.
fn read_session(text: &str) -> Option<SavedSession> {
    session::decode(text, SavedPane::read).filter(|windows| !windows.is_empty())
}

/// How the window looked when the application last ended. Holds nothing secret.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Layout {
    pub width: f64,
    pub height: f64,
    pub maximized: bool,
    pub sidebar_open: bool,
    pub theme: ThemeMode,
    pub panels: Panels,
}

impl Default for Layout {
    fn default() -> Self {
        Self { width: 1280., height: 800., maximized: false, sidebar_open: true, theme: ThemeMode::Dark, panels: Panels::default() }
    }
}

/// Where the application keeps its files.
#[derive(Clone, Debug)]
pub struct Paths {
    pub state: PathBuf,
    pub default_vault: PathBuf,
}

impl Paths {
    /// `RMX_DATA_DIR`, or the user's data directory.
    fn locate() -> Self {
        let home = || std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
        let dir = std::env::var_os("RMX_DATA_DIR").map(PathBuf::from).unwrap_or_else(|| {
            let data = std::env::var_os("XDG_DATA_HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| home().join(".local/share"));
            data.join("rmx")
        });
        Self { state: dir.join("app.local"), default_vault: dir.join("vault.rmxdb") }
    }
}

/// The plaintext file of what belongs to this device only.
pub struct AppState(LocalState);

impl AppState {
    fn open(paths: &Paths) -> Option<Self> {
        let dir = paths.state.parent()?;
        std::fs::create_dir_all(dir).ok()?;
        set_private(dir);
        LocalState::open(&paths.state).ok().map(Self)
    }

    fn layout(&self) -> Layout {
        let number = |key, default| {
            self.0.get(key).ok().flatten().and_then(|v| v.parse().ok()).unwrap_or(default)
        };
        let flag = |key, default| {
            self.0.get(key).ok().flatten().map_or(default, |value| value == "1")
        };
        let theme = self.0.get(KEY_THEME).ok().flatten();
        let defaults = Layout::default();
        Layout {
            width: number(KEY_WIDTH, defaults.width),
            height: number(KEY_HEIGHT, defaults.height),
            maximized: flag(KEY_MAXIMIZED, false),
            sidebar_open: flag(KEY_SIDEBAR, true),
            theme: if theme.as_deref() == Some("light") { ThemeMode::Light } else { ThemeMode::Dark },
            panels: self.0.get(KEY_PANELS).ok().flatten().map_or_else(Panels::default, |text| Panels::read(&text)),
        }
    }

    pub fn save_layout(&self, layout: &Layout) {
        let flag = |value: bool| if value { "1" } else { "0" };
        let theme = if layout.theme == ThemeMode::Light { "light" } else { "dark" };
        let entries = [
            (KEY_WIDTH, layout.width.to_string()),
            (KEY_HEIGHT, layout.height.to_string()),
            (KEY_MAXIMIZED, flag(layout.maximized).to_owned()),
            (KEY_SIDEBAR, flag(layout.sidebar_open).to_owned()),
            (KEY_THEME, theme.to_owned()),
            (KEY_PANELS, layout.panels.spell()),
        ];
        for (key, value) in entries {
            if let Err(err) = self.0.set(key, &value) {
                log::warn!("Could not save the window layout: {err}");
                return;
            }
        }
    }

    fn session(&self) -> Option<SavedSession> {
        read_session(&self.0.get(KEY_SESSION).ok().flatten()?)
    }

    pub fn save_session(&self, text: &str) {
        if let Err(err) = self.0.set(KEY_SESSION, text) {
            log::warn!("Could not save the session: {err}");
        }
    }

    pub fn last_vault(&self) -> Option<PathBuf> {
        self.0.get(KEY_LAST_VAULT).ok().flatten().map(PathBuf::from)
    }

    pub fn set_last_vault(&self, path: &Path) {
        if let Some(text) = path.to_str()
            && let Err(err) = self.0.set(KEY_LAST_VAULT, text)
        {
            log::warn!("Could not remember the vault: {err}");
        }
        let mut recent = self.recent_vaults();
        recent.retain(|known| known != path);
        recent.insert(0, path.to_owned());
        recent.truncate(RECENT_VAULTS);
        let lines: Vec<&str> = recent.iter().filter_map(|known| known.to_str()).collect();
        if let Err(err) = self.0.set(KEY_RECENT_VAULTS, &lines.join("\n")) {
            log::warn!("Could not remember the vault: {err}");
        }
    }

    /// The vaults opened on this device, the latest first.
    pub fn recent_vaults(&self) -> Vec<PathBuf> {
        let text = self.0.get(KEY_RECENT_VAULTS).ok().flatten().unwrap_or_default();
        text.lines().filter(|line| !line.is_empty()).map(PathBuf::from).collect()
    }
}

fn set_private(dir: &Path) {
    use std::os::unix::fs::PermissionsExt;
    if let Err(err) = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)) {
        log::warn!("Could not protect {}: {err}", dir.display());
    }
}

/// What starting up found.
pub struct Boot {
    pub runtime: Runtime,
    pub paths: Paths,
    pub state: Option<AppState>,
    pub layout: Layout,
    /// The windows to reopen once the vault is open.
    pub session: Option<SavedSession>,
}

/// Find everything on a thread of its own, while the compositor is being waited for.
pub fn start() -> JoinHandle<std::io::Result<Boot>> {
    thread::spawn(|| {
        let runtime = Builder::new_multi_thread()
            .worker_threads(WORKERS)
            .thread_name("rmx-session")
            .enable_all()
            .build()?;
        let paths = Paths::locate();
        let state = AppState::open(&paths);
        let layout = state.as_ref().map_or_else(Layout::default, AppState::layout);
        let session = state.as_ref().and_then(AppState::session);
        crate::mark("boot");
        Ok(Boot { runtime, paths, state, layout, session })
    })
}

#[cfg(test)]
mod tests {
    use rmx_layout::session::SavedTab;
    use rmx_layout::split::Split;

    use super::*;

    #[test]
    fn a_session_survives_the_state_file() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths { state: dir.path().join("rmx/app.local"), default_vault: PathBuf::new() };
        let state = AppState::open(&paths).unwrap();
        assert_eq!(state.session(), None);
        let tab = SavedTab { layout: Split::Pane(SavedPane::Node(9)), focus: 0, zoomed: false, name: None };
        let windows = vec![SavedWindow {
            width: 900.,
            height: 600.,
            maximized: false,
            sidebar_open: true,
            active: 0,
            tabs: vec![tab, SavedTab { layout: Split::Pane(SavedPane::Files(7)), focus: 0, zoomed: false, name: None }, SavedTab { layout: Split::Pane(SavedPane::Local), focus: 0, zoomed: false, name: None }],
        }];
        state.save_session(&session::encode(&windows, |pane| pane.spell()));
        drop(state);
        assert_eq!(AppState::open(&paths).unwrap().session(), Some(windows));
    }

    #[test]
    fn a_damaged_or_empty_session_is_nothing_to_restore() {
        assert_eq!(read_session("garbage"), None);
        assert_eq!(read_session("rmx-session 1\n"), None);
    }

    #[test]
    fn the_layout_survives_the_state_file() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths { state: dir.path().join("rmx/app.local"), default_vault: PathBuf::new() };
        let state = AppState::open(&paths).unwrap();
        assert_eq!(state.layout(), Layout::default());

        let layout = Layout {
            width: 900.,
            height: 600.,
            maximized: true,
            sidebar_open: false,
            theme: ThemeMode::Light,
            panels: Panels { sidebar: 321., files_split: 0.3, ..Panels::default() },
        };
        state.save_layout(&layout);
        state.set_last_vault(Path::new("/some/where.rmxdb"));
        drop(state);

        let state = AppState::open(&paths).unwrap();
        assert_eq!(state.layout(), layout);
        assert_eq!(state.last_vault(), Some(PathBuf::from("/some/where.rmxdb")));
    }

    #[test]
    fn the_latest_vault_comes_first_and_once() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths { state: dir.path().join("rmx/app.local"), default_vault: PathBuf::new() };
        let state = AppState::open(&paths).unwrap();
        for name in ["/a", "/b", "/a"] {
            state.set_last_vault(Path::new(name));
        }
        assert_eq!(state.recent_vaults(), [PathBuf::from("/a"), PathBuf::from("/b")]);
        for number in 0..RECENT_VAULTS * 2 {
            state.set_last_vault(Path::new(&format!("/{number}")));
        }
        assert_eq!(state.recent_vaults().len(), RECENT_VAULTS);
    }
}
