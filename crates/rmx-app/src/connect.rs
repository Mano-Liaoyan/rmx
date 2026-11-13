//! Getting a pane connected: everything here runs on the runtime, and talks to the main thread
//! through events.

use rmx_serial::serial::{self, SerialConfig, SerialError};
use rmx_serial::telnet::{self, TelnetOptions};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use rmx_display::{DisplayEvent, DisplayHandle};
use rmx_layout::PaneId;
use rmx_rdp::{CertificatePolicy, CertificatePrompt, RdpConfig, RdpError, Security};
use rmx_render::SizeInfo;
use rmx_ssh::{
    ConnectConfig, ConnectOptions, HostConfig, HostKeyCheck, HostKeyDecision, HostKeyRejection,
    HostKeyVerifier, KeyboardChallenge, PortForward, PromptContext, Prompter, PublicKey,
    SecretString, ShellExit, ShellOptions, ShellReader, ShellWriter, SshClient,
};
use rmx_terminal::{
    CloseReason, Sink, Terminal, Theme, TransportReader, TransportWriter, WindowSize,
};
use rmx_transfer::rustls;
use rmx_transfer::rustls::client::danger::{
    HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier,
};
use rmx_transfer::rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rmx_transfer::{
    DialedStream, FtpConfig, FtpDialer, FtpFs, FtpTls, RemoteFs, SftpFs, SftpOptions, TunnelStream,
};
use rmx_ui::Protocol;
use rmx_ui::widgets::{
    HostKeyStatus, PromptField, RdpQuality, RdpSecurity, ScaleChoice, SessionLogging, VncColors,
    is_rustdesk_address,
};
use rmx_vault::{KnownHost, Tunnel};
use tokio::net::TcpStream;
use tokio::runtime::Handle;
use tokio::sync::{mpsc, oneshot};
use winit::event_loop::EventLoopProxy;

use crate::app::AppEvent;
use crate::background::off_main;
use crate::files::FilesLink;
use crate::remote::{DisplayKind, DisplayLink, Inbox, Incoming};
use crate::store::{KeyHold, Request, VaultHandle};

/// How long a sync waits for an answer about a host key, holding up the vault meanwhile.
const SYNC_PROMPT_TIMEOUT: Duration = Duration::from_secs(120);
/// How long reaching a server may take before giving up on it.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
/// How many passwords may be tried before giving up.
const PASSWORD_ATTEMPTS: u32 = 3;
/// The `known_hosts` type of a pinned RDP server certificate, by its SHA-256.
const RDP_PIN_TYPE: &str = "rdp-tls-sha256";
/// The `known_hosts` type of a pinned FTPS server certificate.
const FTPS_PIN_TYPE: &str = "ftps-tls-sha256";
/// The smallest remote desktop to ask for: RDP refuses less.
const MIN_DESKTOP: u32 = 200;
/// The desktop of a connection that keeps its own size.
const KEPT_DESKTOP: (u16, u16) = (1920, 1080);

/// What the user is asked about a server's key.
pub struct HostKeyPrompt {
    pub status: HostKeyStatus,
    pub host: String,
    pub port: u16,
    pub algorithm: String,
    pub fingerprint: String,
    pub previous: Option<String>,
}

/// The answer to a [`HostKeyPrompt`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HostKeyAnswer {
    Reject,
    Once,
    Always,
}

/// A question for the user, from something running off the main thread.
pub struct Prompt {
    /// The pane it is for; none for a sync.
    pub pane: Option<PaneId>,
    pub kind: PromptKind,
}

pub enum PromptKind {
    HostKey(HostKeyPrompt, oneshot::Sender<HostKeyAnswer>),
    Fields {
        title: String,
        detail: String,
        fields: Vec<PromptField>,
        reply: oneshot::Sender<Option<Vec<SecretString>>>,
    },
}

impl Prompt {
    /// Whether nobody is waiting for the answer any more.
    pub fn abandoned(&self) -> bool {
        match &self.kind {
            PromptKind::HostKey(_, reply) => reply.is_closed(),
            PromptKind::Fields { reply, .. } => reply.is_closed(),
        }
    }
}

/// What a connection, opened from the tree, should end up as.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Goal {
    /// What the connection is for: a shell, a desktop, or files.
    Session,
    /// Browsing its files over a connection of their own, here if a directory is given.
    Files { directory: Option<String> },
}

/// What a session hands its pane once it is up.
pub enum Ready {
    Terminal(Box<Terminal>),
    Display(Box<DisplayLink>),
    Files(Box<FilesLink>),
}

/// What a connection attempt reports about its pane.
pub enum LinkEvent {
    Stage(String),
    /// What it is connecting to, as `user@host:port`.
    Target(String),
    Ready(Ready),
    Failed(String),
    /// A session that was up has ended, for this reason.
    Closed(String),
    /// What it connects to is not on this machine now. Failing, unless the pane was reopened
    /// from the last session, which waits for the user instead.
    Missing(String),
    /// What of the session the connection says to log, sent before it is ready.
    Logging(SessionLogging),
    /// How many port forwards run, and what went wrong with the others.
    Forwards(u32, Vec<String>),
}

/// How a pane reaches the main thread.
#[derive(Clone)]
struct Reporter {
    proxy: EventLoopProxy<AppEvent>,
    pane: PaneId,
    generation: u64,
}

impl Reporter {
    fn send(&self, event: LinkEvent) {
        let _ = self.proxy.send_event(AppEvent::Link(self.pane, self.generation, event));
    }
}

/// Ask `prompt` on the main thread, and wait for the answer. `None` when there will be none.
async fn ask(
    proxy: &EventLoopProxy<AppEvent>,
    pane: Option<PaneId>,
    title: String,
    detail: String,
    labels: Vec<(String, bool)>,
) -> Option<Vec<SecretString>> {
    let (reply, answer) = oneshot::channel();
    let fields = labels
        .into_iter()
        .map(|(label, secret)| PromptField { label, secret, value: String::new() })
        .collect();
    let kind = PromptKind::Fields { title, detail, fields, reply };
    proxy.send_event(AppEvent::Prompt(Prompt { pane, kind })).ok()?;
    answer.await.ok().flatten()
}

/// Asks the user for what authentication needs.
struct UiPrompter {
    proxy: EventLoopProxy<AppEvent>,
    pane: Option<PaneId>,
}

#[async_trait]
impl Prompter for UiPrompter {
    async fn password(&self, context: &PromptContext<'_>) -> Option<SecretString> {
        let title = format!("Password for {}@{}", context.user, context.host);
        let detail = retry_note(context.attempt);
        let answers = ask(&self.proxy, self.pane, title, detail, vec![("Password".into(), true)]);
        answers.await?.into_iter().next()
    }

    async fn key_passphrase(
        &self,
        context: &PromptContext<'_>,
        _path: &Path,
    ) -> Option<SecretString> {
        let title = format!("Key passphrase for {}@{}", context.user, context.host);
        let detail = retry_note(context.attempt);
        let answers = ask(&self.proxy, self.pane, title, detail, vec![("Passphrase".into(), true)]);
        answers.await?.into_iter().next()
    }

    async fn keyboard_interactive(
        &self,
        context: &PromptContext<'_>,
        challenge: &KeyboardChallenge,
    ) -> Option<Vec<SecretString>> {
        let title = if challenge.name.is_empty() {
            format!("Sign in to {}", context.host)
        } else {
            challenge.name.clone()
        };
        let labels = challenge.prompts.iter().map(|p| (p.text.clone(), !p.echo)).collect();
        ask(&self.proxy, self.pane, title, challenge.instructions.clone(), labels).await
    }
}

fn retry_note(attempt: u32) -> String {
    if attempt > 1 { "That was not accepted. Try again.".to_owned() } else { String::new() }
}

/// Where a verifier learns which keys are trusted.
pub enum KnownKeys {
    /// Asks the vault, and records what the user trusts there.
    Vault(VaultHandle),
    /// For syncing, which holds the vault up: these keys were read before it started, and what the
    /// user trusts is collected to be recorded after it ends.
    Snapshot { known: Vec<KnownHost>, trusted: Arc<Mutex<Vec<KnownHost>>> },
}

/// Judges host keys by the vault's `known_hosts`, asking the user about the rest.
pub struct UiVerifier {
    pub proxy: EventLoopProxy<AppEvent>,
    pub pane: Option<PaneId>,
    pub keys: KnownKeys,
    /// Whether anyone is there to ask.
    pub interactive: bool,
}

/// `(algorithm, base64 key)` of `key`, as `known_hosts` has them.
pub fn key_parts(key: &PublicKey) -> Option<(String, String)> {
    let line = key.to_openssh().ok()?;
    let mut fields = line.split_whitespace();
    Some((fields.next()?.to_owned(), fields.next()?.to_owned()))
}

fn fingerprint_of(algorithm: &str, base64: &str) -> Option<String> {
    let key = PublicKey::from_openssh(&format!("{algorithm} {base64}")).ok()?;
    Some(key.fingerprint(Default::default()).to_string())
}

impl UiVerifier {
    async fn known(&self, host: &str, port: u16) -> Result<Vec<KnownHost>, String> {
        match &self.keys {
            KnownKeys::Vault(vault) => {
                let (reply, answer) = oneshot::channel();
                vault.send(Request::KnownHosts { host: host.to_owned(), port, reply });
                answer.await.map_err(|_| "the vault went away".to_owned())?
            },
            KnownKeys::Snapshot { known, .. } => {
                Ok(known.iter().filter(|k| k.host == host && k.port == port).cloned().collect())
            },
        }
    }

    fn trust(&self, row: KnownHost) {
        match &self.keys {
            KnownKeys::Vault(vault) => vault.send(Request::TrustHost(row)),
            KnownKeys::Snapshot { trusted, .. } => {
                if let Ok(mut trusted) = trusted.lock() {
                    trusted.push(row);
                }
            },
        }
    }
}

#[async_trait]
impl HostKeyVerifier for UiVerifier {
    async fn verify(&self, check: &HostKeyCheck<'_>) -> HostKeyDecision {
        let failed = |message: String| HostKeyDecision::Reject(HostKeyRejection::Failed(message));
        let Some((algorithm, base64)) = key_parts(check.key) else {
            return failed("the server's key could not be read".to_owned());
        };
        let known = match self.known(check.host, check.port).await {
            Ok(known) => known,
            Err(err) => return failed(err),
        };
        if known.iter().any(|row| row.key_type == algorithm && row.public_key == base64) {
            return HostKeyDecision::Accept;
        }

        let previous = known
            .iter()
            .find(|row| row.key_type == algorithm)
            .map(|row| fingerprint_of(&row.key_type, &row.public_key).unwrap_or_default());
        let rejection = match &previous {
            Some(fingerprint) => HostKeyRejection::Changed {
                known_fingerprint: fingerprint.clone(),
                location: "the vault".to_owned(),
            },
            None => HostKeyRejection::Unknown,
        };
        if !self.interactive {
            return HostKeyDecision::Reject(rejection);
        }

        let status =
            if previous.is_some() { HostKeyStatus::Changed } else { HostKeyStatus::Unknown };
        let (reply, answer) = oneshot::channel();
        let prompt = HostKeyPrompt {
            status,
            host: check.host.to_owned(),
            port: check.port,
            algorithm: algorithm.clone(),
            fingerprint: check.fingerprint(),
            previous,
        };
        let kind = PromptKind::HostKey(prompt, reply);
        if self.proxy.send_event(AppEvent::Prompt(Prompt { pane: self.pane, kind })).is_err() {
            return HostKeyDecision::Reject(HostKeyRejection::Declined);
        }
        let answer = match self.keys {
            KnownKeys::Vault(_) => answer.await.ok(),
            KnownKeys::Snapshot { .. } => {
                tokio::time::timeout(SYNC_PROMPT_TIMEOUT, answer).await.ok().and_then(Result::ok)
            },
        };
        match answer {
            Some(HostKeyAnswer::Once) => HostKeyDecision::Accept,
            Some(HostKeyAnswer::Always) => {
                self.trust(KnownHost::new(check.host, check.port, &algorithm, &base64));
                HostKeyDecision::Accept
            },
            Some(HostKeyAnswer::Reject) | None => HostKeyDecision::Reject(match rejection {
                HostKeyRejection::Unknown => HostKeyRejection::Declined,
                changed => changed,
            }),
        }
    }
}

/// A shell's output, as a terminal reads it.
struct SshReader(ShellReader);

/// How a shell ended, as a terminal reports it.
fn close_reason(exit: Option<&ShellExit>) -> CloseReason {
    match exit {
        Some(ShellExit::Status(status)) => CloseReason::Exited(*status),
        Some(ShellExit::Signal { name, message, .. }) => {
            CloseReason::Killed { signal: name.clone(), message: message.clone() }
        },
        Some(ShellExit::Disconnected(reason)) => CloseReason::Disconnected(reason.to_string()),
        _ => CloseReason::Closed,
    }
}

impl TransportReader for SshReader {
    async fn read(&mut self) -> Result<Bytes, CloseReason> {
        match self.0.recv().await {
            Some(bytes) => Ok(bytes),
            None => Err(close_reason(self.0.exit())),
        }
    }
}

/// A shell's input, as a terminal writes it.
struct SshWriter(ShellWriter);

impl TransportWriter for SshWriter {
    async fn write(&self, bytes: Bytes) -> Result<(), CloseReason> {
        self.0.write(bytes).await.map_err(|err| CloseReason::Disconnected(err.to_string()))
    }

    async fn resize(&self, size: WindowSize) -> Result<(), CloseReason> {
        let WindowSize { num_cols, num_lines, cell_width, cell_height } = size;
        let (cols, rows) = (u32::from(num_cols), u32::from(num_lines));
        let (width, height) = (cols * u32::from(cell_width), rows * u32::from(cell_height));
        self.0
            .resize(cols, rows, width, height)
            .await
            .map_err(|err| CloseReason::Disconnected(err.to_string()))
    }
}

/// Where a protocol other than SSH connects, and with what.
pub struct Endpoint {
    pub host: String,
    pub port: u16,
    pub user: String,
    pub password: Option<SecretString>,
    pub domain: Option<String>,
}

/// What the vault says about how to reach a connection.
pub struct ConnectPlan {
    pub protocol: Protocol,
    pub endpoint: Endpoint,
    /// How to log in to SSH, for the protocols that run over it.
    pub target: Option<HostConfig>,
    /// The hosts to jump through, the first to dial first.
    pub jumps: Vec<HostConfig>,
    pub tunnels: Vec<Tunnel>,
    /// Private keys handed to the SSH library by path, valid while these live.
    pub holds: Vec<KeyHold>,
    /// How a remote desktop fits its pane.
    pub scale: ScaleChoice,
    pub follow_size: bool,
    pub rdp_security: RdpSecurity,
    pub rdp_quality: RdpQuality,
    /// RDP: what the advanced page of the form set.
    pub rdp: rmx_ui::widgets::RdpAdvanced,
    pub vnc_colors: VncColors,
    /// RustDesk: the rendezvous server, if not RustDesk's public one.
    pub rustdesk_server: Option<String>,
    /// RustDesk: that server's public key, if not the public server's.
    pub rustdesk_key: Option<String>,
    pub ftps: bool,
    pub serial: SerialConfig,
    /// Terminals: what of the session to write to a log file.
    pub log: SessionLogging,
}

/// What a pane needs to get connected.
pub struct ConnectRequest {
    pub pane: PaneId,
    pub generation: u64,
    pub node: u64,
    pub size: SizeInfo,
    /// The pane's content, in pixels, for a desktop to take.
    pub pixels: (u32, u32),
    pub goal: Goal,
    pub theme: Theme,
    pub vault: VaultHandle,
    pub proxy: EventLoopProxy<AppEvent>,
    pub sink: Arc<dyn Sink>,
    pub runtime: Handle,
    pub cancel: oneshot::Receiver<()>,
}

/// Connect, hand the pane what it shows, and keep the connection until told or the server ends it.
pub async fn run(request: ConnectRequest) {
    off_main("connect");
    let ConnectRequest {
        pane,
        generation,
        node,
        size,
        pixels,
        goal,
        theme,
        vault,
        proxy,
        sink,
        runtime,
        mut cancel,
    } = request;
    let reporter = Reporter { proxy: proxy.clone(), pane, generation };

    let (reply, answer) = oneshot::channel();
    vault.send(Request::Plan { node, reply });
    let plan = tokio::select! {
        biased;
        _ = &mut cancel => return,
        plan = answer => plan.map_err(|_| "the vault went away".to_owned()).and_then(|plan| plan),
    };
    let plan = match plan {
        Ok(plan) => plan,
        Err(reason) => {
            reporter.send(LinkEvent::Failed(reason));
            return;
        },
    };
    reporter.send(LinkEvent::Logging(plan.log));

    let session = async {
        match (plan.protocol, goal) {
            (Protocol::Ssh, Goal::Session) => {
                let shell = ShellSpec { size, theme, sink, runtime };
                run_shell(&reporter, plan, shell, vault, &mut cancel).await
            },
            (Protocol::Ssh | Protocol::Sftp, goal) => {
                let directory = match goal {
                    Goal::Files { directory } => directory,
                    Goal::Session => None,
                };
                run_sftp(&reporter, plan, directory, vault, &mut cancel).await
            },
            (Protocol::Ftp, _) => run_ftp(&reporter, plan, vault, &mut cancel).await,
            (Protocol::Vnc, _) => run_vnc(&reporter, plan, vault, &mut cancel).await,
            (Protocol::Rdp, _) => run_rdp(&reporter, plan, pixels, vault, &mut cancel).await,
            (Protocol::RustDesk, _) => run_rustdesk(&reporter, plan, vault, &mut cancel).await,
            (Protocol::Telnet, _) => {
                let shell = ShellSpec { size, theme, sink, runtime };
                run_telnet(&reporter, plan, shell, vault, &mut cancel).await
            },
            (Protocol::Serial, _) => {
                let shell = ShellSpec { size, theme, sink, runtime };
                run_serial(&reporter, plan, shell, &mut cancel).await
            },
        }
    };
    if let Err(reason) = session.await {
        reporter.send(LinkEvent::Failed(reason));
    }
}

/// The verifier and prompter of everything this pane connects through.
fn ssh_config(
    reporter: &Reporter,
    target: HostConfig,
    jumps: Vec<HostConfig>,
    vault: VaultHandle,
) -> ConnectConfig {
    ConnectConfig {
        target,
        jumps,
        verifier: Arc::new(UiVerifier {
            proxy: reporter.proxy.clone(),
            pane: Some(reporter.pane),
            keys: KnownKeys::Vault(vault),
            interactive: true,
        }),
        prompter: Arc::new(UiPrompter { proxy: reporter.proxy.clone(), pane: Some(reporter.pane) }),
        options: ConnectOptions::default(),
    }
}

/// Connect to `plan`'s SSH server, reporting the way there.
async fn connect_ssh(
    reporter: &Reporter,
    target: HostConfig,
    jumps: Vec<HostConfig>,
    vault: VaultHandle,
) -> Result<SshClient, String> {
    reporter.send(LinkEvent::Target(format!("{}@{}:{}", target.user, target.host, target.port)));
    reporter.send(LinkEvent::Stage(match jumps.len() {
        0 => "Connecting".to_owned(),
        1 => "Connecting through a jump host".to_owned(),
        hops => format!("Connecting through {hops} jump hosts"),
    }));
    rmx_ssh::connect(ssh_config(reporter, target, jumps, vault))
        .await
        .map_err(|err| err.to_string())
}

/// What opening a terminal takes.
struct ShellSpec {
    size: SizeInfo,
    theme: Theme,
    sink: Arc<dyn Sink>,
    runtime: Handle,
}

async fn run_shell(
    reporter: &Reporter,
    plan: ConnectPlan,
    shell: ShellSpec,
    vault: VaultHandle,
    cancel: &mut oneshot::Receiver<()>,
) -> Result<(), String> {
    let ConnectPlan { target, jumps, tunnels, holds, .. } = plan;
    let ShellSpec { size, theme, sink, runtime } = shell;
    let target = target.ok_or("this connection has no SSH details")?;
    let setup = async {
        let client = connect_ssh(reporter, target, jumps, vault).await?;
        drop(holds);
        reporter.send(LinkEvent::Stage("Opening a shell".to_owned()));
        let (columns, lines) = (size.columns_count() as u32, size.lines_count() as u32);
        let options = ShellOptions {
            cols: columns,
            rows: lines,
            pixel_width: columns * size.cell_width() as u32,
            pixel_height: lines * size.cell_height() as u32,
            ..ShellOptions::default()
        };
        let (reader, writer) = client.open_shell(&options).await.map_err(|err| err.to_string())?;
        let terminal = Terminal::connect(
            reporter.pane,
            size,
            SshReader(reader),
            SshWriter(writer),
            &runtime,
            theme,
            sink,
        )
        .map_err(|err| err.to_string())?;
        Ok::<_, String>((client, terminal))
    };
    let (client, terminal) = tokio::select! {
        biased;
        _ = &mut *cancel => return Ok(()),
        result = setup => result?,
    };
    reporter.send(LinkEvent::Ready(Ready::Terminal(Box::new(terminal))));

    let (forwards, errors) = start_forwards(&client, &tunnels).await;
    if !tunnels.is_empty() {
        reporter.send(LinkEvent::Forwards(forwards.len() as u32, errors));
    }

    tokio::select! {
        _ = &mut *cancel => client.disconnect().await,
        _ = client.closed() => (),
    }
    for forward in forwards {
        let _ = forward.close().await;
    }
    Ok(())
}

/// Browse files over SFTP, on a connection of its own. A big transfer stops a connection's other
/// channels while it waits for room, and that must not be the shell's.
async fn run_sftp(
    reporter: &Reporter,
    plan: ConnectPlan,
    directory: Option<String>,
    vault: VaultHandle,
    cancel: &mut oneshot::Receiver<()>,
) -> Result<(), String> {
    let ConnectPlan { target, jumps, holds, .. } = plan;
    let target = target.ok_or("this connection has no SSH details")?;
    let title = target.host.clone();
    let setup = async {
        let client = connect_ssh(reporter, target, jumps, vault).await?;
        drop(holds);
        reporter.send(LinkEvent::Stage("Starting SFTP".to_owned()));
        let stream = client.open_sftp().await.map_err(|err| err.to_string())?;
        let fs =
            SftpFs::connect(stream, SftpOptions::default()).await.map_err(|err| err.to_string())?;
        let start = match directory {
            Some(directory) => directory,
            None => fs.canonicalize(".").await.map_err(|err| err.to_string())?,
        };
        Ok::<_, String>((client, fs, start))
    };
    let (client, fs, start) = tokio::select! {
        biased;
        _ = &mut *cancel => return Ok(()),
        result = setup => result?,
    };
    let fs: Arc<SftpFs> = Arc::new(fs);
    reporter.send(LinkEvent::Ready(Ready::Files(Box::new(FilesLink {
        fs: fs.clone() as Arc<dyn RemoteFs>,
        start,
        title,
    }))));
    tokio::select! {
        _ = &mut *cancel => {
            let _ = fs.close();
            client.disconnect().await;
            reporter.send(LinkEvent::Closed("Disconnected".to_owned()));
        },
        reason = client.closed() => {
            reporter.send(LinkEvent::Closed(format!("{reason}")));
        },
    }
    Ok(())
}

/// A tunnel through an SSH jump host for FTP's connections, one channel each.
struct SshDialer(SshClient);

#[async_trait]
impl FtpDialer for SshDialer {
    async fn dial(&self, host: &str, port: u16) -> std::io::Result<DialedStream> {
        let stream = self.0.open_direct_tcpip(host, port).await.map_err(|err| {
            std::io::Error::new(std::io::ErrorKind::ConnectionRefused, err.to_string())
        })?;
        Ok(DialedStream::Tunnel(Box::new(stream)))
    }
}

/// A TLS certificate is accepted only if it is pinned. What is new is noted, for the user to judge.
#[derive(Debug)]
struct PinnedCertificate {
    pins: Vec<[u8; 32]>,
    seen: Arc<Mutex<Option<[u8; 32]>>>,
    provider: Arc<rustls::crypto::CryptoProvider>,
}

fn sha256(der: &[u8]) -> [u8; 32] {
    let digest = ring::digest::digest(&ring::digest::SHA256, der);
    digest.as_ref().try_into().expect("SHA-256 is 32 bytes")
}

impl ServerCertVerifier for PinnedCertificate {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        let hash = sha256(end_entity.as_ref());
        if self.pins.contains(&hash) {
            return Ok(ServerCertVerified::assertion());
        }
        if let Ok(mut seen) = self.seen.lock() {
            *seen = Some(hash);
        }
        Err(rustls::Error::General("the certificate is not pinned".to_owned()))
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.provider.signature_verification_algorithms.supported_schemes()
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn from_hex(text: &str) -> Option<[u8; 32]> {
    let bytes: Option<Vec<u8>> = (0..text.len())
        .step_by(2)
        .map(|at| u8::from_str_radix(text.get(at..at + 2)?, 16).ok())
        .collect();
    bytes?.try_into().ok()
}

/// Ask the user about a certificate. `Some(true)` means trust it from now on.
async fn ask_certificate(
    reporter: &Reporter,
    vault: &VaultHandle,
    pin_type: &str,
    host: &str,
    port: u16,
    sha256: &[u8; 32],
) -> Option<bool> {
    let previous = known_keys(vault, host, port, pin_type).await.into_iter().next();
    let status = if previous.is_some() { HostKeyStatus::Changed } else { HostKeyStatus::Unknown };
    let (reply, answer) = oneshot::channel();
    let prompt = HostKeyPrompt {
        status,
        host: host.to_owned(),
        port,
        algorithm: "TLS certificate (SHA-256)".to_owned(),
        fingerprint: hex(sha256),
        previous,
    };
    let kind = PromptKind::HostKey(prompt, reply);
    reporter.proxy.send_event(AppEvent::Prompt(Prompt { pane: Some(reporter.pane), kind })).ok()?;
    match answer.await.ok()? {
        HostKeyAnswer::Once => Some(false),
        HostKeyAnswer::Always => {
            vault.send(Request::TrustHost(KnownHost::new(host, port, pin_type, &hex(sha256))));
            Some(true)
        },
        HostKeyAnswer::Reject => None,
    }
}

/// The certificates the vault has pinned for this server, as fingerprints.
async fn known_keys(vault: &VaultHandle, host: &str, port: u16, pin_type: &str) -> Vec<String> {
    let (reply, answer) = oneshot::channel();
    vault.send(Request::KnownHosts { host: host.to_owned(), port, reply });
    let rows = answer.await.ok().and_then(Result::ok).unwrap_or_default();
    rows.into_iter().filter(|row| row.key_type == pin_type).map(|row| row.public_key).collect()
}

/// A TLS configuration trusting only `pins`, noting whatever else the server presents.
fn ftps_config(
    pins: Vec<[u8; 32]>,
    seen: Arc<Mutex<Option<[u8; 32]>>>,
) -> Result<rustls::ClientConfig, String> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let verifier = Arc::new(PinnedCertificate { pins, seen, provider: provider.clone() });
    Ok(rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|err| err.to_string())?
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_no_client_auth())
}

async fn run_ftp(
    reporter: &Reporter,
    plan: ConnectPlan,
    vault: VaultHandle,
    cancel: &mut oneshot::Receiver<()>,
) -> Result<(), String> {
    let ConnectPlan { endpoint, jumps, ftps, holds, .. } = plan;
    let Endpoint { host, port, user, password, .. } = endpoint;
    let label =
        if user.is_empty() { format!("{host}:{port}") } else { format!("{user}@{host}:{port}") };
    reporter.send(LinkEvent::Target(label));
    let password = password.map(|secret| secret.expose().to_owned()).unwrap_or_default();

    let setup = async {
        let tunnel = open_tunnel(reporter, jumps, vault.clone()).await?;
        drop(holds);
        let dialer: Arc<dyn FtpDialer> = match &tunnel {
            Some(client) => Arc::new(SshDialer(client.clone())),
            None => Arc::new(rmx_transfer::TcpDialer),
        };
        reporter.send(LinkEvent::Stage("Logging in".to_owned()));
        let user = if user.is_empty() { "anonymous".to_owned() } else { user };
        let mut config = FtpConfig::new(host.clone(), port, user, password);
        if !ftps {
            let fs = FtpFs::connect_with(config, dialer).await.map_err(|err| err.to_string())?;
            return Ok::<_, String>(fs);
        }

        let known = known_keys(&vault, &host, port, FTPS_PIN_TYPE).await;
        let mut pins: Vec<[u8; 32]> = known.iter().filter_map(|text| from_hex(text)).collect();
        for _ in 0..2 {
            let seen: Arc<Mutex<Option<[u8; 32]>>> = Arc::default();
            let tls = ftps_config(pins.clone(), seen.clone())?;
            config.tls = Some(FtpTls { config: Arc::new(tls), server_name: host.clone() });
            match FtpFs::connect_with(config.clone(), dialer.clone()).await {
                Ok(fs) => return Ok(fs),
                Err(err) => {
                    let noted = seen.lock().ok().and_then(|mut seen| seen.take());
                    let Some(hash) = noted else { return Err(err.to_string()) };
                    match ask_certificate(reporter, &vault, FTPS_PIN_TYPE, &host, port, &hash).await
                    {
                        Some(_) => pins.push(hash),
                        None => return Err("the server's certificate was not trusted".to_owned()),
                    }
                },
            }
        }
        Err("the server's certificate could not be verified".to_owned())
    };
    let fs = tokio::select! {
        biased;
        _ = &mut *cancel => return Ok(()),
        result = setup => result?,
    };
    let title = "Remote".to_owned();
    let start = fs.home().to_owned();
    reporter.send(LinkEvent::Ready(Ready::Files(Box::new(FilesLink {
        fs: Arc::new(fs),
        start,
        title,
    }))));
    // FTP has no way to report that the server left: the pane finds out from the next listing.
    let _ = (&mut *cancel).await;
    reporter.send(LinkEvent::Closed("Disconnected".to_owned()));
    Ok(())
}

/// A terminal's output, ending when told to: a session has nothing else to hang up with.
struct Stoppable<R> {
    inner: R,
    stop: oneshot::Receiver<()>,
}

impl<R: TransportReader> TransportReader for Stoppable<R> {
    async fn read(&mut self) -> Result<Bytes, CloseReason> {
        tokio::select! {
            biased;
            _ = &mut self.stop => Err(CloseReason::Closed),
            result = self.inner.read() => result,
        }
    }
}

/// A plain terminal over TCP, through a tunnel if there are jumps.
async fn run_telnet(
    reporter: &Reporter,
    plan: ConnectPlan,
    shell: ShellSpec,
    vault: VaultHandle,
    cancel: &mut oneshot::Receiver<()>,
) -> Result<(), String> {
    let ConnectPlan { endpoint: Endpoint { host, port, .. }, jumps, .. } = plan;
    let ShellSpec { size, theme, sink, runtime } = shell;
    reporter.send(LinkEvent::Target(format!("{host}:{port}")));
    let setup = async {
        let tunnel = open_tunnel(reporter, jumps, vault).await?;
        let stream = open_stream(tunnel.as_ref(), &host, port).await?;
        Ok::<_, String>((tunnel, stream))
    };
    let (tunnel, stream) = tokio::select! {
        biased;
        _ = &mut *cancel => return Ok(()),
        result = setup => result?,
    };
    let options = TelnetOptions {
        cols: size.columns_count() as u16,
        rows: size.lines_count() as u16,
        ..TelnetOptions::default()
    };
    let (reader, writer) = telnet::connect(stream, options, &runtime);
    let (stop_sender, stop) = oneshot::channel();
    let reader = Stoppable { inner: reader, stop };
    let terminal = Terminal::connect(reporter.pane, size, reader, writer, &runtime, theme, sink)
        .map_err(|err| err.to_string())?;
    reporter.send(LinkEvent::Ready(Ready::Terminal(Box::new(terminal))));

    let _ = (&mut *cancel).await;
    let _ = stop_sender.send(());
    drop(tunnel);
    Ok(())
}

/// What to tell the user about a port that would not open.
fn serial_problem(path: &str, error: &SerialError) -> String {
    match error {
        SerialError::NotFound => format!("{path} is not on this machine"),
        SerialError::PermissionDenied => {
            format!(
                "Not allowed to open {path}: add yourself to the dialout group and log in again"
            )
        },
        SerialError::Busy => format!("{path} is in use by another program"),
        SerialError::NotASerialPort => format!("{path} is not a serial port"),
        other => format!("Could not open {path}: {other}"),
    }
}

/// A terminal on a local serial port. Opening it can fail in ways only this machine can say.
async fn run_serial(
    reporter: &Reporter,
    plan: ConnectPlan,
    shell: ShellSpec,
    cancel: &mut oneshot::Receiver<()>,
) -> Result<(), String> {
    let ConnectPlan { endpoint: Endpoint { host: path, .. }, serial: config, .. } = plan;
    let ShellSpec { size, theme, sink, runtime } = shell;
    reporter.send(LinkEvent::Target(path.clone()));
    reporter.send(LinkEvent::Stage("Opening the port".to_owned()));
    let opened = tokio::select! {
        biased;
        _ = &mut *cancel => return Ok(()),
        opened = serial::open(&path, config) => opened,
    };
    let (reader, writer) = match opened {
        Ok(ends) => ends,
        Err(SerialError::NotFound) => {
            reporter.send(LinkEvent::Missing(serial_problem(&path, &SerialError::NotFound)));
            return Ok(());
        },
        Err(error) => return Err(serial_problem(&path, &error)),
    };
    let (stop_sender, stop) = oneshot::channel();
    let reader = Stoppable { inner: reader, stop };
    let terminal = Terminal::connect(reporter.pane, size, reader, writer, &runtime, theme, sink)
        .map_err(|err| err.to_string())?;
    reporter.send(LinkEvent::Ready(Ready::Terminal(Box::new(terminal))));

    let _ = (&mut *cancel).await;
    let _ = stop_sender.send(());
    Ok(())
}

/// Reach the first of `jumps`' hosts: the last hop is the one to tunnel through.
async fn open_tunnel(
    reporter: &Reporter,
    mut jumps: Vec<HostConfig>,
    vault: VaultHandle,
) -> Result<Option<SshClient>, String> {
    let Some(target) = jumps.pop() else {
        reporter.send(LinkEvent::Stage("Connecting".to_owned()));
        return Ok(None);
    };
    let label = format!("{}@{}:{}", target.user, target.host, target.port);
    reporter.send(LinkEvent::Stage(format!("Reaching {label}")));
    let client = rmx_ssh::connect(ssh_config(reporter, target, jumps, vault))
        .await
        .map_err(|err| err.to_string())?;
    Ok(Some(client))
}

/// A connection to the server itself, straight or through `tunnel`.
async fn open_stream(
    tunnel: Option<&SshClient>,
    host: &str,
    port: u16,
) -> Result<Box<dyn TunnelStream>, String> {
    let connect = async {
        match tunnel {
            Some(client) => {
                let stream =
                    client.open_direct_tcpip(host, port).await.map_err(|err| err.to_string())?;
                Ok::<Box<dyn TunnelStream>, String>(Box::new(stream))
            },
            None => {
                let stream =
                    TcpStream::connect((host, port)).await.map_err(|err| err.to_string())?;
                let _ = stream.set_nodelay(true);
                Ok::<Box<dyn TunnelStream>, String>(Box::new(stream))
            },
        }
    };
    tokio::time::timeout(CONNECT_TIMEOUT, connect)
        .await
        .map_err(|_| "the server did not answer".to_owned())?
}

/// Read a session's events, delivering frames as the main thread can take them.
async fn pump_display(
    reporter: &Reporter,
    kind: DisplayKind,
    handle: DisplayHandle,
    scale: ScaleChoice,
    follow_size: bool,
    tunnel: Option<SshClient>,
    cancel: &mut oneshot::Receiver<()>,
) {
    let DisplayHandle { mut events, input } = handle;
    let inbox = Arc::new(Inbox::default());
    let (pane, generation) = (reporter.pane, reporter.generation);
    let link = DisplayLink { kind, input: input.clone(), inbox: inbox.clone(), scale, follow_size };
    reporter.send(LinkEvent::Ready(Ready::Display(Box::new(link))));

    let proxy = reporter.proxy.clone();
    let tell = move || {
        let _ = proxy.send_event(AppEvent::DisplayNews(pane, generation));
    };
    let reason = loop {
        let event = tokio::select! {
            biased;
            _ = &mut *cancel => {
                let _ = input.send(rmx_display::InputEvent::Disconnect).await;
                break Some("Disconnected".to_owned());
            },
            event = events.recv() => event,
        };
        let incoming = match event {
            None => break Some("The session ended".to_owned()),
            Some(DisplayEvent::Resized { width, height }) => Incoming::Resized { width, height },
            Some(DisplayEvent::Update(update)) => Incoming::Update(update),
            Some(DisplayEvent::Clipboard(text)) => {
                let _ =
                    reporter.proxy.send_event(AppEvent::DisplayClipboard(pane, generation, text));
                continue;
            },
            Some(DisplayEvent::Cursor(cursor)) => {
                let _ =
                    reporter.proxy.send_event(AppEvent::DisplayCursor(pane, generation, cursor));
                continue;
            },
            Some(DisplayEvent::Bell) => continue,
            Some(DisplayEvent::Closed(reason)) => {
                break Some(reason.unwrap_or_else(|| "The session ended".to_owned()));
            },
        };
        if inbox.push(incoming).await {
            tell();
        }
    };
    if let Some(reason) = reason {
        reporter.send(LinkEvent::Closed(reason));
    }
    if let Some(client) = tunnel {
        client.disconnect().await;
    }
}

async fn run_vnc(
    reporter: &Reporter,
    plan: ConnectPlan,
    vault: VaultHandle,
    cancel: &mut oneshot::Receiver<()>,
) -> Result<(), String> {
    let ConnectPlan { endpoint, jumps, holds, scale, follow_size, vnc_colors, .. } = plan;
    let Endpoint { host, port, password, .. } = endpoint;
    let color_depth = match vnc_colors {
        VncColors::Full => rmx_vnc::ColorDepth::Full,
        VncColors::Bits16 => rmx_vnc::ColorDepth::Bits16,
        VncColors::Bits8 => rmx_vnc::ColorDepth::Bits8,
    };
    reporter.send(LinkEvent::Target(format!("{host}:{port}")));
    let mut password = password.map(|secret| secret.expose().to_owned());

    let setup = async {
        let tunnel = open_tunnel(reporter, jumps, vault).await?;
        drop(holds);
        reporter.send(LinkEvent::Stage("Starting the session".to_owned()));
        for attempt in 0..PASSWORD_ATTEMPTS {
            let stream = open_stream(tunnel.as_ref(), &host, port).await?;
            let config = rmx_vnc::Config {
                password: password.clone(),
                color_depth,
                ..rmx_vnc::Config::default()
            };
            let connect = tokio::time::timeout(CONNECT_TIMEOUT, rmx_vnc::connect(stream, config));
            let result = connect.await.map_err(|_| "the server did not answer".to_owned())?;
            match result {
                Ok(handle) => return Ok::<_, String>((handle, tunnel)),
                Err(
                    rmx_vnc::ConnectError::PasswordRequired | rmx_vnc::ConnectError::WrongPassword,
                ) => {
                    let title = format!("VNC password for {host}");
                    let detail = retry_note(attempt + 1);
                    let labels = vec![("Password".to_owned(), true)];
                    let answer =
                        ask(&reporter.proxy, Some(reporter.pane), title, detail, labels).await;
                    let Some(typed) = answer.and_then(|answers| answers.into_iter().next()) else {
                        return Err("A password is needed".to_owned());
                    };
                    password = Some(typed.expose().to_owned());
                },
                Err(err) => return Err(err.to_string()),
            }
        }
        Err("The password was not accepted".to_owned())
    };
    let (handle, tunnel) = tokio::select! {
        biased;
        _ = &mut *cancel => return Ok(()),
        result = setup => result?,
    };
    pump_display(reporter, DisplayKind::Vnc, handle, scale, follow_size, tunnel, cancel).await;
    Ok(())
}

/// Judge the server certificates a session presents.
async fn judge_certificates(
    reporter: Reporter,
    vault: VaultHandle,
    host: String,
    port: u16,
    mut prompts: mpsc::Receiver<CertificatePrompt>,
) {
    while let Some(prompt) = prompts.recv().await {
        let CertificatePrompt { sha256, answer, .. } = prompt;
        let known = known_keys(&vault, &host, port, RDP_PIN_TYPE).await;
        if known.iter().any(|pin| pin == &hex(&sha256)) {
            let _ = answer.send(true);
            continue;
        }
        let trusted = ask_certificate(&reporter, &vault, RDP_PIN_TYPE, &host, port, &sha256).await;
        let _ = answer.send(trusted.is_some());
    }
}

/// A desktop size RDP will take, nearest to what the pane has room for.
fn rdp_size(pixels: (u32, u32)) -> (u16, u16) {
    let side = |length: u32| (length.clamp(MIN_DESKTOP, 8192) & !1) as u16;
    (side(pixels.0), side(pixels.1))
}

async fn run_rdp(
    reporter: &Reporter,
    plan: ConnectPlan,
    pixels: (u32, u32),
    vault: VaultHandle,
    cancel: &mut oneshot::Receiver<()>,
) -> Result<(), String> {
    let ConnectPlan {
        endpoint,
        jumps,
        holds,
        scale,
        follow_size,
        rdp_security,
        rdp_quality,
        rdp,
        ..
    } = plan;
    let Endpoint { host, port, user, password, domain } = endpoint;
    reporter.send(LinkEvent::Target(format!("{user}@{host}:{port}")));
    let mut password = password.map(|secret| secret.expose().to_owned());

    let setup = async {
        let tunnel = open_tunnel(reporter, jumps, vault.clone()).await?;
        drop(holds);
        reporter.send(LinkEvent::Stage("Starting the session".to_owned()));
        let (width, height) = if follow_size { rdp_size(pixels) } else { KEPT_DESKTOP };
        let security = match rdp_security {
            RdpSecurity::Negotiate => Security::NlaOrTls,
            RdpSecurity::Nla => Security::Nla,
            RdpSecurity::Tls => Security::Tls,
        };

        for attempt in 0..PASSWORD_ATTEMPTS {
            let secret = match &password {
                Some(secret) => secret.clone(),
                None => {
                    let title = format!("Password for {user}@{host}");
                    let labels = vec![("Password".to_owned(), true)];
                    let answer = ask(
                        &reporter.proxy,
                        Some(reporter.pane),
                        title,
                        retry_note(attempt + 1),
                        labels,
                    )
                    .await;
                    let typed = answer.and_then(|answers| answers.into_iter().next());
                    typed.ok_or("A password is needed")?.expose().to_owned()
                },
            };
            let (prompts_tx, prompts) = mpsc::channel(1);
            let mut config =
                RdpConfig::new(host.clone(), user.clone(), secret, (width, height), [0; 32]);
            // Asking is the only way a certificate is judged here: nothing is accepted unseen.
            config.certificate = CertificatePolicy::Ask(prompts_tx);
            config.security = security;
            crate::rdp_options::apply(rdp_quality, &rdp, &mut config);
            config.domain = domain.clone();
            config.connect_timeout = CONNECT_TIMEOUT;

            let stream = open_stream(tunnel.as_ref(), &host, port).await?;
            let judge =
                judge_certificates(reporter.clone(), vault.clone(), host.clone(), port, prompts);
            let connect = rmx_rdp::connect(stream, config);
            tokio::pin!(judge);
            tokio::pin!(connect);
            let result = tokio::select! {
                result = &mut connect => result,
                () = &mut judge => connect.await,
            };
            match result {
                Ok(handle) => return Ok::<_, String>((handle, tunnel)),
                Err(RdpError::Handshake(message))
                    if password.is_some() && is_auth_failure(&message) =>
                {
                    password = None;
                },
                Err(RdpError::CertificateRejected) => {
                    return Err("The server's certificate was not trusted".to_owned());
                },
                Err(err) => return Err(err.to_string()),
            }
        }
        Err("The password was not accepted".to_owned())
    };
    let (handle, tunnel) = tokio::select! {
        biased;
        _ = &mut *cancel => return Ok(()),
        result = setup => result?,
    };
    pump_display(reporter, DisplayKind::Rdp, handle, scale, follow_size, tunnel, cancel).await;
    Ok(())
}

/// The rendezvous server a RustDesk connection uses when it names none.
const RUSTDESK_PUBLIC_SERVER: &str = "rs-ny.rustdesk.com";

/// Opens what a RustDesk session dials, straight or through the jump hosts' tunnel.
struct RustDeskDialer<'a> {
    tunnel: Option<&'a SshClient>,
}

impl rmx_rustdesk::Dial for RustDeskDialer<'_> {
    fn dial<'a>(
        &'a self,
        host: &'a str,
        port: u16,
    ) -> rmx_rustdesk::BoxFuture<'a, std::io::Result<Box<dyn rmx_rustdesk::Transport>>> {
        Box::pin(async move {
            let stream =
                open_stream(self.tunnel, host, port).await.map_err(std::io::Error::other)?;
            Ok(Box::new(stream) as Box<dyn rmx_rustdesk::Transport>)
        })
    }
}

/// Where a RustDesk connection goes: an address straight, an ID through its server.
fn rustdesk_target(
    host: &str,
    port: u16,
    server: Option<String>,
    key: Option<String>,
) -> rmx_rustdesk::Target {
    if is_rustdesk_address(host) {
        return rmx_rustdesk::Target::Direct { host: host.trim().to_owned(), port };
    }
    let server = rmx_rustdesk::Server {
        host: server.unwrap_or_else(|| RUSTDESK_PUBLIC_SERVER.to_owned()),
        key,
    };
    rmx_rustdesk::Target::Id { id: host.trim().to_owned(), server }
}

async fn run_rustdesk(
    reporter: &Reporter,
    plan: ConnectPlan,
    vault: VaultHandle,
    cancel: &mut oneshot::Receiver<()>,
) -> Result<(), String> {
    use rmx_rustdesk::ConnectError;

    let ConnectPlan {
        endpoint,
        jumps,
        holds,
        scale,
        rdp_quality,
        rustdesk_server,
        rustdesk_key,
        ..
    } = plan;
    let Endpoint { host, port, password, .. } = endpoint;
    let target = rustdesk_target(&host, port, rustdesk_server, rustdesk_key);
    reporter.send(LinkEvent::Target(match &target {
        rmx_rustdesk::Target::Id { id, server } => format!("{id} via {}", server.host),
        rmx_rustdesk::Target::Direct { host, port } => format!("{host}:{port}"),
    }));
    let quality = match rdp_quality {
        RdpQuality::Best => rmx_rustdesk::Quality::Best,
        RdpQuality::Balanced | RdpQuality::Custom => rmx_rustdesk::Quality::Balanced,
        RdpQuality::Fast => rmx_rustdesk::Quality::Fast,
    };
    let mut credentials = rmx_rustdesk::Credentials {
        password: password.map(|secret| secret.expose().to_owned()),
        two_factor_code: None,
    };

    let setup = async {
        let tunnel = open_tunnel(reporter, jumps, vault).await?;
        drop(holds);
        let dialer = RustDeskDialer { tunnel: tunnel.as_ref() };
        for attempt in 0..PASSWORD_ATTEMPTS {
            reporter.send(LinkEvent::Stage(match (&target, &credentials.password) {
                (_, None) => "Waiting for the peer to accept".to_owned(),
                (rmx_rustdesk::Target::Id { .. }, Some(_)) => "Reaching the peer".to_owned(),
                (rmx_rustdesk::Target::Direct { .. }, Some(_)) => "Starting the session".to_owned(),
            }));
            let mut config = rmx_rustdesk::Config::new(target.clone());
            config.credentials = credentials.clone();
            config.quality = quality;
            let (title, label) = match rmx_rustdesk::connect(&dialer, config).await {
                Ok(session) => {
                    match session.identity {
                        rmx_rustdesk::Identity::Verified => {},
                        rmx_rustdesk::Identity::Unverified => {
                            log::warn!(
                                "the RustDesk peer at {host} is encrypted but its identity was not checked"
                            )
                        },
                        rmx_rustdesk::Identity::Unencrypted => {
                            log::warn!(
                                "the RustDesk session with {host} is not encrypted, as RustDesk serves direct connections"
                            )
                        },
                    }
                    return Ok::<_, String>((session.display, tunnel));
                },
                Err(ConnectError::PasswordRequired | ConnectError::WrongPassword) => {
                    (format!("RustDesk password for {host}"), "Password")
                },
                Err(ConnectError::TwoFactorRequired | ConnectError::WrongTwoFactorCode) => {
                    (format!("Two-factor code for {host}"), "Code")
                },
                Err(err) => return Err(err.to_string()),
            };
            let detail = retry_note(attempt + 1);
            let secret = label == "Password";
            let answer = ask(
                &reporter.proxy,
                Some(reporter.pane),
                title,
                detail,
                vec![(label.to_owned(), secret)],
            )
            .await;
            let typed = answer
                .and_then(|answers| answers.into_iter().next())
                .ok_or("An answer is needed")?;
            if secret {
                credentials.password = Some(typed.expose().to_owned());
            } else {
                credentials.two_factor_code = Some(typed.expose().trim().to_owned());
            }
        }
        Err("The peer did not accept the login".to_owned())
    };
    let (handle, tunnel) = tokio::select! {
        biased;
        _ = &mut *cancel => return Ok(()),
        result = setup => result?,
    };
    // Never `follow_size`: a RustDesk peer's resolution is not the client's to change.
    pump_display(reporter, DisplayKind::RustDesk, handle, scale, false, tunnel, cancel).await;
    Ok(())
}

/// Whether an RDP handshake failed because of the credentials rather than the network.
fn is_auth_failure(message: &str) -> bool {
    let text = message.to_ascii_lowercase();
    ["logon", "credential", "authentication", "password"].iter().any(|word| text.contains(word))
}

/// Start the connection's port forwards. A forward that cannot start leaves the session standing.
async fn start_forwards(client: &SshClient, tunnels: &[Tunnel]) -> (Vec<PortForward>, Vec<String>) {
    let (mut running, mut errors) = (Vec::new(), Vec::new());
    for tunnel in tunnels.iter().filter(|tunnel| tunnel.autostart) {
        let bind = format!("{}:{}", tunnel.bind_host, tunnel.bind_port);
        let target = tunnel.target_host.as_deref().zip(tunnel.target_port);
        let started = match (tunnel.kind.as_str(), target) {
            ("local", Some((host, port))) => client.forward_local(bind, host, port).await,
            ("remote", Some((host, port))) => {
                client.forward_remote(&tunnel.bind_host, tunnel.bind_port, host, port).await
            },
            ("dynamic", _) => client.forward_dynamic(bind).await,
            _ => {
                errors.push(format!("{}: unknown kind of forward", tunnel.name));
                continue;
            },
        };
        match started {
            Ok(forward) => running.push(forward),
            Err(err) => errors.push(format!("{}: {err}", tunnel.name)),
        }
    }
    (running, errors)
}

/// The dialogs and policy a sync connection uses.
pub fn sync_verifier(
    proxy: EventLoopProxy<AppEvent>,
    known: Vec<KnownHost>,
    interactive: bool,
) -> (Arc<UiVerifier>, Arc<Mutex<Vec<KnownHost>>>) {
    let trusted: Arc<Mutex<Vec<KnownHost>>> = Arc::default();
    let keys = KnownKeys::Snapshot { known, trusted: trusted.clone() };
    (Arc::new(UiVerifier { proxy, pane: None, keys, interactive }), trusted)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_rustdesk_id_goes_through_its_server_and_an_address_straight() {
        let by_id = rustdesk_target(" 123456789 ", 21118, None, None);
        let server = rmx_rustdesk::Server { host: RUSTDESK_PUBLIC_SERVER.to_owned(), key: None };
        assert_eq!(by_id, rmx_rustdesk::Target::Id { id: "123456789".to_owned(), server });

        let own = rustdesk_target("office", 1, Some("rd.example.com".into()), Some("KEY".into()));
        let server =
            rmx_rustdesk::Server { host: "rd.example.com".to_owned(), key: Some("KEY".into()) };
        assert_eq!(own, rmx_rustdesk::Target::Id { id: "office".to_owned(), server });

        for host in ["192.168.1.20", "pc.lan", "fe80::1"] {
            let direct = rustdesk_target(host, 21118, Some("ignored".into()), None);
            assert_eq!(direct, rmx_rustdesk::Target::Direct { host: host.to_owned(), port: 21118 });
        }
    }

    #[test]
    fn certificate_fingerprints_survive_their_stored_form() {
        let hash = sha256(b"a certificate");
        assert_eq!(from_hex(&hex(&hash)), Some(hash));
        assert_eq!(from_hex("abc"), None);
    }

    #[test]
    fn a_desktop_is_never_asked_to_be_too_small_or_odd() {
        assert_eq!(rdp_size((1919, 1081)), (1918, 1080));
        assert_eq!(rdp_size((10, 10)), (200, 200));
        assert_eq!(rdp_size((20000, 900)), (8192, 900));
    }

    #[test]
    fn an_unpinned_certificate_is_noted_and_refused() {
        let seen: Arc<Mutex<Option<[u8; 32]>>> = Arc::default();
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let verifier = PinnedCertificate { pins: Vec::new(), seen: seen.clone(), provider };
        let der = CertificateDer::from(b"der".to_vec());
        let name = ServerName::try_from("host").unwrap();
        let verdict = verifier.verify_server_cert(&der, &[], &name, &[], UnixTime::now());
        assert!(verdict.is_err());
        assert_eq!(*seen.lock().unwrap(), Some(sha256(b"der")));

        let pinned = PinnedCertificate {
            pins: vec![sha256(b"der")],
            seen: Arc::default(),
            provider: Arc::new(rustls::crypto::ring::default_provider()),
        };
        assert!(pinned.verify_server_cert(&der, &[], &name, &[], UnixTime::now()).is_ok());
    }

    #[test]
    fn every_way_a_shell_ends_reaches_the_pane() {
        assert_eq!(close_reason(Some(&ShellExit::Status(3))), CloseReason::Exited(3));
        assert_eq!(close_reason(Some(&ShellExit::Closed)), CloseReason::Closed);
        assert_eq!(close_reason(None), CloseReason::Closed);
        let signal =
            ShellExit::Signal { name: "TERM".into(), core_dumped: false, message: String::new() };
        let reason = close_reason(Some(&signal));
        assert_eq!(reason, CloseReason::Killed { signal: "TERM".into(), message: String::new() });
    }

    #[test]
    fn a_key_splits_into_what_known_hosts_stores() {
        const LINE: &str =
            "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIMIRiy7ZfFV9wAKHiSxTvfwgpvxcDVqrrPnFBnbtxvEG";
        let key = PublicKey::from_openssh(LINE).unwrap();
        let (algorithm, base64) = key_parts(&key).unwrap();
        assert_eq!(algorithm, "ssh-ed25519");
        assert_eq!(format!("{algorithm} {base64}"), LINE);
        assert_eq!(
            fingerprint_of(&algorithm, &base64).unwrap(),
            key.fingerprint(Default::default()).to_string()
        );
    }
}
