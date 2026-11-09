use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::pin::pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use bytes::Bytes;
use russh::client::{ChannelOpenHandle, Handle, Handler, Msg, Session};
use russh::keys::{PublicKey, PublicKeyOrCertificate};
use russh::{Channel, ChannelMsg, Disconnect, Preferred};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpStream;
use tokio::sync::{Mutex, mpsc, watch};

use crate::auth::{AuthMethod, Authenticator, Prompter};
use crate::error::{DisconnectReason, Error};
use crate::host_key::{HostKeyCheck, HostKeyDecision, HostKeyRejection, HostKeyVerifier};
use crate::shell::{self, ShellOptions, ShellReader, ShellWriter};
use crate::stream::SshStream;

/// One machine to log in to: the target or a jump host.
#[derive(Clone, Debug)]
pub struct HostConfig {
    /// Host name or address. For a host behind a jump, this is resolved by the
    /// previous hop, not locally.
    pub host: String,
    /// TCP port.
    pub port: u16,
    /// User to log in as.
    pub user: String,
    /// Authentication methods, tried in order until one succeeds.
    pub auth: Vec<AuthMethod>,
}

impl HostConfig {
    /// A host on the default SSH port.
    pub fn new(host: impl Into<String>, user: impl Into<String>, auth: Vec<AuthMethod>) -> Self {
        Self { host: host.into(), port: 22, user: user.into(), auth }
    }

    /// Uses `port` instead of 22.
    pub fn with_port(mut self, port: u16) -> Self {
        self.port = port;
        self
    }
}

/// Tuning for a connection. [`Default`] suits an interactive session.
#[derive(Clone, Debug)]
pub struct ConnectOptions {
    /// Limit on each network step: the TCP connect, the handshake and each
    /// authentication round trip. Time waiting on a [`Prompter`] or
    /// [`HostKeyVerifier`] does not count.
    pub timeout: Duration,
    /// How often to probe a silent server. `None` disables keepalives.
    pub keepalive_interval: Option<Duration>,
    /// Unanswered keepalives before giving up. Zero means never give up.
    pub keepalive_max: usize,
    /// Close the connection after this long with no traffic.
    pub inactivity_timeout: Option<Duration>,
    /// Packets each channel can queue before it holds up the connection.
    pub channel_buffer: usize,
    /// Offer zlib compression. Rarely worthwhile on a fast link.
    pub compression: bool,
    /// How many times to ask a [`Prompter`] when an answer is wrong.
    pub prompt_attempts: u32,
}

impl Default for ConnectOptions {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(20),
            keepalive_interval: Some(Duration::from_secs(30)),
            keepalive_max: 3,
            inactivity_timeout: None,
            channel_buffer: 64,
            compression: false,
            prompt_attempts: 3,
        }
    }
}

/// Everything [`connect`] needs.
#[derive(Clone)]
pub struct ConnectConfig {
    /// The machine to end up on.
    pub target: HostConfig,
    /// Jump hosts to pass through, nearest first, like `ssh -J a,b target`.
    pub jumps: Vec<HostConfig>,
    /// Judges each hop's host key.
    pub verifier: Arc<dyn HostKeyVerifier>,
    /// Answers authentication prompts for every hop.
    pub prompter: Arc<dyn Prompter>,
    /// Applied to every hop.
    pub options: ConnectOptions,
}

impl fmt::Debug for ConnectConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ConnectConfig")
            .field("target", &self.target)
            .field("jumps", &self.jumps)
            .field("options", &self.options)
            .finish_non_exhaustive()
    }
}

/// Connects and authenticates, through any jump hosts in `config`.
///
/// # Errors
///
/// A failure at a jump host is wrapped in [`Error::Jump`] naming that hop.
pub async fn connect(config: ConnectConfig) -> Result<SshClient, Error> {
    let mut previous: Option<SshClient> = None;
    for hop in &config.jumps {
        let client = connect_hop(hop, &config, previous.take()).await.map_err(|source| {
            Error::Jump { host: hop.host.clone(), port: hop.port, source: Box::new(source) }
        })?;
        previous = Some(client);
    }
    connect_hop(&config.target, &config, previous).await
}

async fn connect_hop(
    hop: &HostConfig,
    config: &ConnectConfig,
    via: Option<SshClient>,
) -> Result<SshClient, Error> {
    match via {
        Some(via) => {
            let stream = via.open_direct_tcpip(&hop.host, hop.port).await?;
            establish(stream, hop, config, Some(via)).await
        },
        None => {
            let timeout = config.options.timeout;
            let address = (hop.host.as_str(), hop.port);
            let stream = match tokio::time::timeout(timeout, TcpStream::connect(address)).await {
                Ok(Ok(stream)) => stream,
                Ok(Err(source)) => {
                    return Err(Error::Connect { host: hop.host.clone(), port: hop.port, source });
                },
                Err(_) => {
                    return Err(Error::Timeout {
                        host: hop.host.clone(),
                        port: hop.port,
                        stage: "TCP connect",
                    });
                },
            };
            // Terminal echo is latency-sensitive and SSH packets are already framed.
            if let Err(error) = stream.set_nodelay(true) {
                log::debug!("set_nodelay failed: {error}");
            }
            establish(stream, hop, config, None).await
        },
    }
}

fn russh_config(options: &ConnectOptions) -> Arc<russh::client::Config> {
    Arc::new(russh::client::Config {
        keepalive_interval: options.keepalive_interval,
        keepalive_max: options.keepalive_max,
        inactivity_timeout: options.inactivity_timeout,
        channel_buffer_size: options.channel_buffer,
        preferred: preferred_algorithms(options.compression, hardware_aes_gcm()),
        ..Default::default()
    })
}

fn preferred_algorithms(compression: bool, hardware_aes_gcm: bool) -> Preferred {
    let base = if compression { Preferred::COMPRESSED } else { Preferred::DEFAULT };
    if !hardware_aes_gcm {
        return base;
    }
    // `ring`'s ChaCha20-Poly1305 reaches about 1.7 GiB/s on a current x86-64
    // core and its AES-GCM about 12 GiB/s, and the session task runs the cipher
    // serially, so a chacha-first list costs bulk transfers a large share of
    // their throughput. Only worth doing with hardware AES and carry-less
    // multiply; without them software AES is slower than ChaCha20.
    let mut ciphers = vec![russh::cipher::AES_256_GCM];
    ciphers.extend(base.cipher.iter().filter(|c| **c != russh::cipher::AES_256_GCM));
    Preferred { cipher: ciphers.into(), ..base }
}

fn hardware_aes_gcm() -> bool {
    #[cfg(target_arch = "x86_64")]
    {
        is_x86_feature_detected!("aes") && is_x86_feature_detected!("pclmulqdq")
    }
    #[cfg(target_arch = "aarch64")]
    {
        std::arch::is_aarch64_feature_detected!("aes")
            && std::arch::is_aarch64_feature_detected!("pmull")
    }
    #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
    {
        false
    }
}

async fn establish<S>(
    stream: S,
    hop: &HostConfig,
    config: &ConnectConfig,
    via: Option<SshClient>,
) -> Result<SshClient, Error>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let (reason_tx, reason) = watch::channel(None);
    let shared = Arc::new(Shared {
        reason: reason_tx,
        requested: AtomicBool::new(false),
        awaiting_decision: AtomicBool::new(false),
        routes: Mutex::new(HashMap::new()),
    });
    let handler = ClientHandler {
        shared: Arc::clone(&shared),
        verifier: Arc::clone(&config.verifier),
        prompter: Arc::clone(&config.prompter),
        host: hop.host.clone(),
        port: hop.port,
        accepted: None,
    };

    let timeout = config.options.timeout;
    let mut handshake =
        pin!(russh::client::connect_stream(russh_config(&config.options), stream, handler));
    let mut handle = loop {
        tokio::select! {
            result = &mut handshake => break result?,
            () = tokio::time::sleep(timeout) => {
                if !shared.awaiting_decision.load(Ordering::Relaxed) {
                    return Err(Error::Timeout {
                        host: hop.host.clone(),
                        port: hop.port,
                        stage: "SSH handshake",
                    });
                }
            }
        }
    };

    Authenticator {
        handle: &mut handle,
        host: hop,
        prompter: &config.prompter,
        timeout,
        prompt_attempts: config.options.prompt_attempts,
    }
    .run()
    .await?;

    Ok(SshClient {
        inner: Arc::new(Inner {
            handle,
            shared,
            reason,
            host: hop.host.clone(),
            port: hop.port,
            user: hop.user.clone(),
            via,
        }),
    })
}

/// A forwarded connection the server wants to open on us.
pub(crate) struct Incoming {
    pub channel: Channel<Msg>,
    pub reply: ChannelOpenHandle,
}

/// Remote-forward listeners, by the address and port the server reports.
pub(crate) type Routes = Mutex<HashMap<(String, u32), mpsc::Sender<Incoming>>>;

/// State shared between a client and its russh session task.
pub(crate) struct Shared {
    reason: watch::Sender<Option<DisconnectReason>>,
    requested: AtomicBool,
    awaiting_decision: AtomicBool,
    pub routes: Routes,
}

impl Shared {
    /// Records why the session ended. The first report wins.
    fn finish(&self, reason: DisconnectReason) {
        let reason = if self.requested.load(Ordering::Relaxed) {
            DisconnectReason::Requested
        } else {
            reason
        };
        self.reason.send_if_modified(|current| {
            if current.is_none() {
                *current = Some(reason);
                true
            } else {
                false
            }
        });
    }
}

pub(crate) struct ClientHandler {
    shared: Arc<Shared>,
    verifier: Arc<dyn HostKeyVerifier>,
    prompter: Arc<dyn Prompter>,
    host: String,
    port: u16,
    /// The key approved for this session; a rekey must present the same one.
    accepted: Option<PublicKey>,
}

impl Drop for ClientHandler {
    fn drop(&mut self) {
        self.shared.finish(DisconnectReason::ConnectionClosed);
    }
}

fn reason_from_error(error: Error) -> DisconnectReason {
    match error {
        Error::Disconnected(reason) => reason,
        Error::Io(source) if source.kind() == std::io::ErrorKind::UnexpectedEof => {
            DisconnectReason::ConnectionClosed
        },
        other => DisconnectReason::Transport(other.to_string()),
    }
}

impl ClientHandler {
    fn rejected(&self, key: &PublicKey, reason: HostKeyRejection) -> Error {
        let check = HostKeyCheck { host: &self.host, port: self.port, key };
        Error::HostKeyRejected {
            host: self.host.clone(),
            port: self.port,
            fingerprint: check.fingerprint(),
            reason,
        }
    }
}

impl Handler for ClientHandler {
    type Error = Error;

    async fn auth_banner(&mut self, banner: &str, _session: &mut Session) -> Result<(), Error> {
        self.prompter.banner(&self.host, banner).await;
        Ok(())
    }

    async fn check_server_key(
        &mut self,
        server_public_key: &PublicKeyOrCertificate,
    ) -> Result<bool, Error> {
        let key = match server_public_key {
            PublicKeyOrCertificate::PublicKey { key, .. } => key.clone(),
            // Not negotiated by default; the certified key is still what the
            // server proved it holds, so judge that.
            PublicKeyOrCertificate::Certificate(certificate) => {
                PublicKey::from(certificate.public_key().clone())
            },
        };
        if let Some(accepted) = &self.accepted {
            if accepted.key_data() == key.key_data() {
                return Ok(true);
            }
            let known = HostKeyCheck { host: &self.host, port: self.port, key: accepted };
            let reason = HostKeyRejection::Changed {
                known_fingerprint: known.fingerprint(),
                location: String::from("this session"),
            };
            return Err(self.rejected(&key, reason));
        }

        self.shared.awaiting_decision.store(true, Ordering::Relaxed);
        let check = HostKeyCheck { host: &self.host, port: self.port, key: &key };
        let decision = self.verifier.verify(&check).await;
        self.shared.awaiting_decision.store(false, Ordering::Relaxed);
        match decision {
            HostKeyDecision::Accept => {
                self.accepted = Some(key);
                Ok(true)
            },
            HostKeyDecision::Reject(reason) => Err(self.rejected(&key, reason)),
        }
    }

    async fn server_channel_open_forwarded_tcpip(
        &mut self,
        channel: Channel<Msg>,
        connected_address: &str,
        connected_port: u32,
        _originator_address: &str,
        _originator_port: u32,
        reply: ChannelOpenHandle,
        _session: &mut Session,
    ) -> Result<(), Error> {
        let shared = Arc::clone(&self.shared);
        let key = (connected_address.to_owned(), connected_port);
        // A task, because the route table can be locked while a forward is
        // being registered, and that registration needs this session task to
        // keep running. Dropping `incoming` rejects the channel.
        tokio::spawn(async move {
            let incoming = Incoming { channel, reply };
            let route = {
                let routes = shared.routes.lock().await;
                routes
                    .get(&key)
                    .or_else(|| routes.iter().find(|(k, _)| k.1 == key.1).map(|(_, v)| v))
                    .cloned()
            };
            if let Some(route) = route {
                let _ = route.send(incoming).await;
            }
        });
        Ok(())
    }

    async fn disconnected(
        &mut self,
        reason: russh::client::DisconnectReason<Error>,
    ) -> Result<(), Error> {
        self.shared.finish(match reason {
            russh::client::DisconnectReason::ReceivedDisconnect(info) => {
                DisconnectReason::Server { code: info.reason_code as u32, message: info.message }
            },
            russh::client::DisconnectReason::Error(error) => reason_from_error(error),
        });
        Ok(())
    }
}

struct Inner {
    handle: Handle<ClientHandler>,
    shared: Arc<Shared>,
    reason: watch::Receiver<Option<DisconnectReason>>,
    host: String,
    port: u16,
    user: String,
    via: Option<SshClient>,
}

/// An authenticated SSH connection. Cheap to clone; all clones share it.
///
/// The connection ends when [`disconnect`](Self::disconnect) is called or the
/// last clone, channel, stream and forward are gone. Everything opened from a
/// client keeps it alive, and a client keeps its jump hosts alive.
#[derive(Clone)]
pub struct SshClient {
    inner: Arc<Inner>,
}

impl fmt::Debug for SshClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SshClient")
            .field("host", &self.inner.host)
            .field("port", &self.inner.port)
            .field("user", &self.inner.user)
            .finish_non_exhaustive()
    }
}

impl SshClient {
    /// The host this connection is to.
    pub fn host(&self) -> &str {
        &self.inner.host
    }

    /// The port this connection is to.
    pub fn port(&self) -> u16 {
        self.inner.port
    }

    /// The user logged in as.
    pub fn user(&self) -> &str {
        &self.inner.user
    }

    /// The jump host this connection runs through, if any. Its own
    /// `jump_host` gives the hop before that.
    pub fn jump_host(&self) -> Option<&SshClient> {
        self.inner.via.as_ref()
    }

    /// Opens a PTY shell.
    ///
    /// # Errors
    ///
    /// [`Error::RequestDenied`] if the server refuses the PTY or the shell.
    pub async fn open_shell(
        &self,
        options: &ShellOptions,
    ) -> Result<(ShellReader, ShellWriter), Error> {
        shell::open(self, options).await
    }

    /// Opens a tunnel to `host:port` as seen from the server.
    ///
    /// # Errors
    ///
    /// [`Error::ChannelOpen`] if the server refuses or cannot reach the target.
    pub async fn open_direct_tcpip(&self, host: &str, port: u16) -> Result<SshStream, Error> {
        self.open_direct_tcpip_from(host, port, "127.0.0.1", 0).await
    }

    pub(crate) async fn open_direct_tcpip_from(
        &self,
        host: &str,
        port: u16,
        originator_address: &str,
        originator_port: u32,
    ) -> Result<SshStream, Error> {
        let channel = self
            .inner
            .handle
            .channel_open_direct_tcpip(host, u32::from(port), originator_address, originator_port)
            .await
            .map_err(|error| self.classify_open(error, format!("{host}:{port}")))?;
        Ok(SshStream::new(channel.into_stream(), self.clone()))
    }

    /// Opens the `sftp` subsystem, returning a stream to hand to an SFTP client.
    ///
    /// # Errors
    ///
    /// [`Error::RequestDenied`] if the server has no SFTP subsystem.
    pub async fn open_sftp(&self) -> Result<SshStream, Error> {
        let mut channel = self.open_session_channel().await?;
        channel.request_subsystem(true, "sftp").await.map_err(|e| self.classify(e))?;
        let mut unexpected = VecDeque::new();
        self.wait_channel_reply(&mut channel, &mut unexpected, "SFTP subsystem").await?;
        if !unexpected.is_empty() {
            return Err(Error::Protocol(String::from("data received before the SFTP handshake")));
        }
        Ok(SshStream::new(channel.into_stream(), self.clone()))
    }

    /// Ends the connection and waits for it to close. Jump hosts stay up while
    /// something else is using them.
    pub async fn disconnect(&self) {
        self.inner.shared.requested.store(true, Ordering::Relaxed);
        let sent = self.inner.handle.disconnect(Disconnect::ByApplication, "", "en").await;
        if sent.is_ok() {
            // The send only queues the message; a wedged peer must not hold us.
            let _ = tokio::time::timeout(Duration::from_secs(5), self.closed()).await;
        }
    }

    /// Waits until the connection has ended and says why.
    pub async fn closed(&self) -> DisconnectReason {
        let mut receiver = self.inner.reason.clone();
        let _ = receiver.wait_for(Option::is_some).await;
        self.disconnect_reason().unwrap_or(DisconnectReason::ConnectionClosed)
    }

    /// Why the connection ended, or `None` while it is still up.
    pub fn disconnect_reason(&self) -> Option<DisconnectReason> {
        let reason = self.inner.reason.borrow().clone()?;
        if reason == DisconnectReason::Requested {
            return Some(reason);
        }
        if let Some(via) = &self.inner.via
            && let Some(parent) = via.disconnect_reason()
        {
            return Some(DisconnectReason::JumpHostLost {
                host: via.inner.host.clone(),
                port: via.inner.port,
                reason: Box::new(parent),
            });
        }
        Some(reason)
    }

    pub(crate) fn handle(&self) -> &Handle<ClientHandler> {
        &self.inner.handle
    }

    pub(crate) fn shared(&self) -> &Arc<Shared> {
        &self.inner.shared
    }

    pub(crate) async fn open_session_channel(&self) -> Result<Channel<Msg>, Error> {
        self.inner
            .handle
            .channel_open_session()
            .await
            .map_err(|error| self.classify_open(error, String::from("session")))
    }

    /// Converts a russh error, preferring the known reason if the session is gone.
    pub(crate) fn classify(&self, error: russh::Error) -> Error {
        match self.disconnect_reason() {
            Some(reason) => Error::Disconnected(reason),
            None => Error::from(error),
        }
    }

    fn classify_open(&self, error: russh::Error, destination: String) -> Error {
        match self.classify(error) {
            Error::ChannelOpen { reason, .. } => Error::ChannelOpen { destination, reason },
            other => other,
        }
    }

    /// Waits for the answer to a `want_reply` channel request. Output that
    /// arrives first is kept in `early` so that none is lost.
    pub(crate) async fn wait_channel_reply(
        &self,
        channel: &mut Channel<Msg>,
        early: &mut VecDeque<Bytes>,
        what: &str,
    ) -> Result<(), Error> {
        loop {
            match channel.wait().await {
                Some(ChannelMsg::Success) => return Ok(()),
                Some(ChannelMsg::Failure) => return Err(Error::RequestDenied(what.to_owned())),
                Some(ChannelMsg::Data { data }) => early.push_back(data),
                Some(ChannelMsg::Close) | None => {
                    return Err(Error::Disconnected(
                        self.disconnect_reason().unwrap_or(DisconnectReason::ConnectionClosed),
                    ));
                },
                Some(_) => {},
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use russh::cipher::{AES_256_GCM, CHACHA20_POLY1305};

    use super::*;

    #[test]
    fn hardware_aes_puts_gcm_first_and_keeps_the_rest() {
        let list = preferred_algorithms(false, true).cipher;
        assert_eq!(list[0], AES_256_GCM);
        assert_eq!(list.iter().filter(|c| **c == AES_256_GCM).count(), 1);
        assert_eq!(list.len(), Preferred::DEFAULT.cipher.len());
        assert!(list.contains(&CHACHA20_POLY1305));
    }

    #[test]
    fn without_hardware_aes_the_order_is_unchanged() {
        assert_eq!(preferred_algorithms(false, false).cipher, Preferred::DEFAULT.cipher);
        assert_eq!(
            preferred_algorithms(true, false).compression,
            Preferred::COMPRESSED.compression
        );
    }

    #[test]
    fn compression_choice_survives_the_cipher_reorder() {
        assert_eq!(preferred_algorithms(true, true).compression, Preferred::COMPRESSED.compression);
    }
}
