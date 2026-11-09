//! [`RemoteFs`] over FTP and explicit FTPS.
//!
//! # Tunnelling
//!
//! `suppaftp` builds its control and passive data connections from `tokio::net::TcpStream` and
//! has no way to accept another stream type. An [`FtpDialer`] may still return any byte stream:
//! a [`DialedStream::Tunnel`] is joined to a private loopback TCP socket pair, and the library sees
//! the loopback end. TLS for FTPS is negotiated inside the tunnel, so the stream carries only
//! ciphertext.
//!
//! The port a server names in a passive reply is always dialed on the configured host, never the
//! address the server provided. This keeps the data connection on the same route as the control
//! connection, works behind NAT, and stops a hostile server from redirecting the client elsewhere.
//!
//! # Connections
//!
//! An FTP connection runs one transfer at a time, and listings count as transfers. [`FtpFs`]
//! keeps a pool of up to [`FtpConfig::max_connections`] connections, each opened on demand and
//! held only for the length of the operation.

use std::future::Future;
use std::net::Ipv4Addr;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, UNIX_EPOCH};

use async_trait::async_trait;
use bytes::{Bytes, BytesMut};
use rustls::client::Resumption;
use suppaftp::list::{File as FtpFile, ListParser};
use suppaftp::tokio::{AsyncRustlsConnector, AsyncRustlsFtpStream, TransferStream};
use suppaftp::types::FileType;
use suppaftp::{FtpError, Mode, Status};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio::time::timeout;
use tokio_rustls::TlsConnector;

use crate::error::{Error, Result};
use crate::fs::{
    BatchBuffer, DirStream, Entry, EntryKind, ListOptions, RangeRead, RangeWrite, RemoteFs,
    WriteStart, base_name, join,
};

/// Read chunk size; matches the buffer `suppaftp` data streams are read through.
const CHUNK: usize = 256 * 1024;
/// An idle pooled connection older than this is checked with `NOOP` before reuse.
const REVALIDATE_AFTER: Duration = Duration::from_secs(20);

/// A byte stream usable as an FTP connection.
pub trait TunnelStream: AsyncRead + AsyncWrite + Send + Unpin {}
impl<T: AsyncRead + AsyncWrite + Send + Unpin> TunnelStream for T {}

/// A connection returned by an [`FtpDialer`].
pub enum DialedStream {
    /// A direct TCP connection; used without any bridging.
    Tcp(TcpStream),
    /// Any other stream, for example an SSH `direct-tcpip` channel.
    Tunnel(Box<dyn TunnelStream>),
}

impl std::fmt::Debug for DialedStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            DialedStream::Tcp(_) => "DialedStream::Tcp",
            DialedStream::Tunnel(_) => "DialedStream::Tunnel",
        })
    }
}

/// Opens the connections an FTP session needs.
#[async_trait]
pub trait FtpDialer: Send + Sync + 'static {
    /// Connects to `host:port`. Called for the control connection and for every data connection.
    async fn dial(&self, host: &str, port: u16) -> std::io::Result<DialedStream>;
}

/// Connects directly over TCP.
#[derive(Debug, Clone, Copy, Default)]
pub struct TcpDialer;

#[async_trait]
impl FtpDialer for TcpDialer {
    async fn dial(&self, host: &str, port: u16) -> std::io::Result<DialedStream> {
        Ok(DialedStream::Tcp(TcpStream::connect((host, port)).await?))
    }
}

/// TLS settings for explicit FTPS (`AUTH TLS`).
#[derive(Clone)]
pub struct FtpTls {
    /// Trust roots and protocol settings. The caller decides what to trust.
    pub config: Arc<rustls::ClientConfig>,
    /// Name the server certificate must match.
    pub server_name: String,
}

impl std::fmt::Debug for FtpTls {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FtpTls").field("server_name", &self.server_name).finish_non_exhaustive()
    }
}

/// Connection settings for [`FtpFs`].
#[derive(Clone)]
pub struct FtpConfig {
    /// Server host name, passed to the dialer for every connection.
    pub host: String,
    /// Control port, normally 21.
    pub port: u16,
    /// Login name.
    pub username: String,
    /// Login password.
    pub password: String,
    /// Upgrades to FTPS when set.
    pub tls: Option<FtpTls>,
    /// Connections opened at most; also the number of simultaneous transfers.
    pub max_connections: usize,
    /// Uses `EPSV` instead of `PASV`.
    pub extended_passive: bool,
    /// Time allowed for a single control command.
    pub command_timeout: Duration,
    /// Longest pause tolerated in a data connection.
    pub data_timeout: Duration,
    /// How long an operation waits for a free connection before failing with [`Error::Busy`].
    /// Finite so that transfers sharing one pool cannot wait on each other forever.
    pub acquire_timeout: Duration,
}

impl std::fmt::Debug for FtpConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FtpConfig")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("username", &self.username)
            .field("password", &"<redacted>")
            .field("tls", &self.tls)
            .field("max_connections", &self.max_connections)
            .finish_non_exhaustive()
    }
}

impl FtpConfig {
    /// Creates settings with sensible limits, no TLS and four connections.
    pub fn new(
        host: impl Into<String>,
        port: u16,
        username: impl Into<String>,
        password: impl Into<String>,
    ) -> Self {
        Self {
            host: host.into(),
            port,
            username: username.into(),
            password: password.into(),
            tls: None,
            max_connections: 4,
            extended_passive: false,
            command_timeout: Duration::from_secs(30),
            data_timeout: Duration::from_secs(60),
            acquire_timeout: Duration::from_secs(30),
        }
    }
}

/// A pool of FTP connections to one server.
pub struct FtpFs {
    shared: Arc<Shared>,
}

impl std::fmt::Debug for FtpFs {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FtpFs").field("config", &self.shared.config).finish_non_exhaustive()
    }
}

struct Shared {
    config: FtpConfig,
    dialer: Arc<dyn FtpDialer>,
    idle: Mutex<Vec<(AsyncRustlsFtpStream, Instant)>>,
    permits: Arc<Semaphore>,
    /// Directory the server started the session in; relative paths are resolved against it.
    home: String,
    has_mlst: bool,
}

/// Connects a dialed stream to a `TcpStream`, bridging through loopback if it is a tunnel.
async fn into_tcp(stream: DialedStream) -> std::io::Result<TcpStream> {
    let tunnel = match stream {
        DialedStream::Tcp(tcp) => return Ok(tcp),
        DialedStream::Tunnel(tunnel) => tunnel,
    };
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
    let client = TcpStream::connect(listener.local_addr()?).await?;
    let client_addr = client.local_addr()?;
    // Only our own socket may attach; another local process could race for the port.
    let server = loop {
        let (candidate, peer) = listener.accept().await?;
        if peer == client_addr {
            break candidate;
        }
        log::warn!("ignored unexpected connection to FTP tunnel bridge from {peer}");
    };
    client.set_nodelay(true)?;
    server.set_nodelay(true)?;
    tokio::spawn(async move {
        let mut server = server;
        let mut tunnel = tunnel;
        let _ = tokio::io::copy_bidirectional(&mut server, &mut tunnel).await;
    });
    Ok(client)
}

fn check_path(path: &str) -> Result<()> {
    if path.chars().any(|c| matches!(c, '\r' | '\n' | '\0')) {
        return Err(Error::InvalidPath(format!("{path:?} contains a control character")));
    }
    Ok(())
}

fn map_error(context: &str, error: FtpError) -> Error {
    let message = format!("{context}: {error}");
    match &error {
        FtpError::ConnectionError(io) => {
            Error::from_io(context, std::io::Error::new(io.kind(), io.to_string()))
        },
        FtpError::UnexpectedResponse(response) => match response.status {
            Status::NotAvailable | Status::TransferAborted => Error::Connection(message),
            status => classify_reply(message, status == Status::FileUnavailable),
        },
        _ => Error::Protocol(message),
    }
}

/// FTP has one reply code for most failures, so the distinction is only in the text.
fn classify_reply(message: String, file_unavailable: bool) -> Error {
    let lower = message.to_ascii_lowercase();
    let missing = lower.contains("no such")
        || lower.contains("not found")
        || (file_unavailable
            && (lower.contains("failed to open") || lower.contains("failed to change directory")));
    if lower.contains("permission") || lower.contains("denied") {
        Error::PermissionDenied(message)
    } else if missing {
        Error::NotFound(message)
    } else {
        Error::Protocol(message)
    }
}

/// Whether the connection can be reused after this failure.
fn keeps_connection(error: &FtpError) -> bool {
    matches!(error, FtpError::UnexpectedResponse(response)
        if !matches!(response.status, Status::NotAvailable | Status::TransferAborted))
}

impl Shared {
    async fn open_connection(
        config: &FtpConfig,
        dialer: &Arc<dyn FtpDialer>,
    ) -> Result<AsyncRustlsFtpStream> {
        let context = format!("{}:{}", config.host, config.port);
        let connect = async {
            let stream = dialer
                .dial(&config.host, config.port)
                .await
                .map_err(|e| Error::from_io(&context, e))?;
            let stream = into_tcp(stream).await.map_err(|e| Error::from_io(&context, e))?;
            let _ = stream.set_nodelay(true);
            let mut ftp = AsyncRustlsFtpStream::connect_with_stream(stream)
                .await
                .map_err(|e| map_error(&context, e))?;
            if let Some(tls) = &config.tls {
                // A server may require each data connection to resume the TLS session of its own
                // control connection, so sessions must not be shared between pooled connections.
                let mut tls_config = (*tls.config).clone();
                tls_config.resumption = Resumption::in_memory_sessions(4);
                let connector =
                    AsyncRustlsConnector::from(TlsConnector::from(Arc::new(tls_config)));
                ftp = ftp
                    .into_secure(connector, &tls.server_name)
                    .await
                    .map_err(|e| map_error(&context, e))?;
            }
            ftp.login(&config.username, &config.password)
                .await
                .map_err(|e| map_error(&context, e))?;
            ftp.transfer_type(FileType::Binary).await.map_err(|e| map_error(&context, e))?;
            ftp.set_mode(if config.extended_passive {
                Mode::ExtendedPassive
            } else {
                Mode::Passive
            });
            // With the workaround on, `PASV` keeps the control peer's address, so only its port is
            // used; `passive_stream_builder` below replaces the address in any case.
            ftp.set_passive_nat_workaround(true);
            let dialer = Arc::clone(dialer);
            let host = config.host.clone();
            Ok::<_, Error>(ftp.passive_stream_builder(move |address| {
                let dialer = Arc::clone(&dialer);
                let host = host.clone();
                // The library requires a `Sync` future, so run the dial on its own task; a join
                // handle is `Sync` whatever the dialer's future is.
                let task = tokio::spawn(async move {
                    let stream = dialer.dial(&host, address.port()).await?;
                    let stream = into_tcp(stream).await?;
                    let _ = stream.set_nodelay(true);
                    Ok::<_, std::io::Error>(stream)
                });
                Box::pin(async move {
                    task.await
                        .map_err(|e| std::io::Error::other(e.to_string()))
                        .and_then(|result| result)
                        .map_err(FtpError::ConnectionError)
                }) as Pin<Box<dyn Future<Output = _> + Send + Sync>>
            }))
        };
        match timeout(config.command_timeout, connect).await {
            Ok(result) => result,
            Err(_) => Err(Error::Timeout(context)),
        }
    }

    /// Takes a connection from the pool, opening one if none is usable.
    async fn lease(self: &Arc<Self>) -> Result<Lease> {
        let permit =
            match timeout(self.config.acquire_timeout, Arc::clone(&self.permits).acquire_owned())
                .await
            {
                Ok(Ok(permit)) => permit,
                Ok(Err(_)) => return Err(Error::Connection("FTP pool closed".into())),
                Err(_) => return Err(Error::Busy(self.config.host.clone())),
            };
        loop {
            let entry = self.idle.lock().expect("pool lock").pop();
            let Some((mut ftp, returned)) = entry else {
                let ftp = Shared::open_connection(&self.config, &self.dialer).await?;
                return Ok(Lease::new(Arc::clone(self), ftp, permit));
            };
            if returned.elapsed() < REVALIDATE_AFTER
                || matches!(timeout(self.config.command_timeout, ftp.noop()).await, Ok(Ok(())))
            {
                return Ok(Lease::new(Arc::clone(self), ftp, permit));
            }
        }
    }

    fn absolute(&self, path: &str) -> Result<String> {
        check_path(path)?;
        Ok(if path.starts_with('/') { path.to_owned() } else { join(&self.home, path) })
    }

    fn settle<T>(
        &self,
        context: &str,
        lease: &mut Lease,
        result: std::result::Result<std::result::Result<T, FtpError>, tokio::time::error::Elapsed>,
    ) -> Result<T> {
        match result {
            Ok(Ok(value)) => Ok(value),
            Ok(Err(error)) => {
                if !keeps_connection(&error) {
                    lease.discard();
                }
                Err(map_error(context, error))
            },
            Err(_) => {
                lease.discard();
                Err(Error::Timeout(context.to_owned()))
            },
        }
    }
}

/// A connection taken from the pool. Returned on drop unless marked broken.
struct Lease {
    ftp: Option<AsyncRustlsFtpStream>,
    shared: Arc<Shared>,
    _permit: OwnedSemaphorePermit,
}

impl Lease {
    fn new(shared: Arc<Shared>, ftp: AsyncRustlsFtpStream, permit: OwnedSemaphorePermit) -> Self {
        Self { ftp: Some(ftp), shared, _permit: permit }
    }

    fn ftp(&mut self) -> &mut AsyncRustlsFtpStream {
        self.ftp.as_mut().expect("lease holds a connection until dropped")
    }

    fn discard(&mut self) {
        self.ftp = None;
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        if let Some(ftp) = self.ftp.take() {
            self.shared.idle.lock().expect("pool lock").push((ftp, Instant::now()));
        }
    }
}

impl FtpFs {
    /// Connects and logs in over TCP.
    pub async fn connect(config: FtpConfig) -> Result<Self> {
        Self::connect_with(config, Arc::new(TcpDialer)).await
    }

    /// Connects and logs in with connections from `dialer`, so that FTP can run through a tunnel.
    pub async fn connect_with(config: FtpConfig, dialer: Arc<dyn FtpDialer>) -> Result<Self> {
        let mut ftp = Shared::open_connection(&config, &dialer).await?;
        let home = ftp.pwd().await.map_err(|e| map_error("PWD", e))?;
        let has_mlst = ftp.feat().await.is_ok_and(|features| features.contains_key("MLST"));
        let permits = Arc::new(Semaphore::new(config.max_connections.max(1)));
        let shared = Arc::new(Shared {
            config,
            dialer,
            idle: Mutex::new(vec![(ftp, Instant::now())]),
            permits,
            home,
            has_mlst,
        });
        Ok(Self { shared })
    }

    /// The directory the server put the login in, as `PWD` said when connecting.
    pub fn home(&self) -> &str {
        &self.shared.home
    }

    /// Reads metadata with `SIZE`, for servers without `MLST`.
    async fn stat_with_size(&self, path: &str, lease: &mut Lease) -> Result<Entry> {
        let shared = &self.shared;
        let limit = shared.config.command_timeout;
        let size = match timeout(limit, lease.ftp().size(path)).await {
            // `SIZE` refuses directories, which `CWD` then tells apart from a missing path. Every
            // command uses an absolute path, so moving the working directory affects nothing.
            Ok(Err(error)) if keeps_connection(&error) => {
                let result = timeout(limit, lease.ftp().cwd(path)).await;
                return match shared.settle(path, lease, result) {
                    Ok(()) => Ok(Entry {
                        name: base_name(path),
                        kind: EntryKind::Dir,
                        size: None,
                        modified: None,
                        mode: None,
                    }),
                    Err(Error::Protocol(_) | Error::NotFound(_)) => {
                        Err(Error::NotFound(path.to_owned()))
                    },
                    Err(error) => Err(error),
                };
            },
            result => shared.settle(path, lease, result)?,
        };
        let modified = match timeout(limit, lease.ftp().mdtm(path)).await {
            Ok(Ok(time)) => {
                Some(UNIX_EPOCH + Duration::from_secs(time.and_utc().timestamp().max(0) as u64))
            },
            _ => None,
        };
        Ok(Entry {
            name: base_name(path),
            kind: EntryKind::File,
            size: Some(size as u64),
            modified,
            mode: None,
        })
    }
}

fn entry_from_list(file: &FtpFile) -> Entry {
    let kind = if file.is_symlink() {
        EntryKind::Symlink
    } else if file.is_directory() {
        EntryKind::Dir
    } else if file.is_file() {
        EntryKind::File
    } else {
        EntryKind::Other
    };
    Entry {
        name: file.name().to_owned(),
        kind,
        size: Some(file.size() as u64),
        modified: Some(file.modified()),
        mode: None,
    }
}

/// Parses one line of `LIST` or `MLSD` output, or `None` for lines such as `total 12`.
fn parse_list_line(line: &str, mlsd: bool) -> Option<FtpFile> {
    if mlsd {
        ListParser::parse_mlsd(line).ok()
    } else {
        ListParser::parse_posix(line).or_else(|_| ListParser::parse_dos(line)).ok()
    }
}

#[async_trait]
impl RemoteFs for FtpFs {
    async fn list(&self, path: &str, options: ListOptions) -> Result<DirStream> {
        let path = self.shared.absolute(path)?;
        let limit = self.shared.config.command_timeout;
        let data_limit = self.shared.config.data_timeout;
        let shared = Arc::clone(&self.shared);
        let mut lease = shared.lease().await?;
        // Change directory first. A path given to `LIST` would be subject to the server's option
        // parsing and globbing.
        let result = timeout(limit, lease.ftp().cwd(&path)).await;
        shared.settle(&path, &mut lease, result)?;

        let mlsd = shared.has_mlst;
        let codes = [Status::AboutToSend, Status::AlreadyOpen];
        let opened = if mlsd {
            timeout(limit, lease.ftp().custom_data_command("MLSD", &codes)).await
        } else {
            // `-a` reveals dot-files on servers that honour it. A refusal falls back to plain
            // `LIST`.
            match timeout(limit, lease.ftp().custom_data_command("LIST -a", &codes)).await {
                Ok(Err(error)) if keeps_connection(&error) => {
                    timeout(limit, lease.ftp().custom_data_command("LIST", &codes)).await
                },
                other => other,
            }
        };
        let (_, stream) = shared.settle(&path, &mut lease, opened)?;

        let (tx, dir) = DirStream::channel();
        tokio::spawn(async move {
            let mut reader = BufReader::new(stream);
            let mut buffer = BatchBuffer::new(options);
            let mut line = Vec::new();
            loop {
                line.clear();
                match timeout(data_limit, reader.read_until(b'\n', &mut line)).await {
                    Ok(Ok(0)) => break,
                    Ok(Ok(_)) => {
                        let text = String::from_utf8_lossy(&line);
                        let Some(file) = parse_list_line(text.trim_end_matches(['\r', '\n']), mlsd)
                        else {
                            continue;
                        };
                        if matches!(file.name(), "." | "..") {
                            continue;
                        }
                        if let Some(batch) = buffer.push(entry_from_list(&file))
                            && tx.send(Ok(batch)).await.is_err()
                        {
                            // The consumer left. The transfer cannot be reused half-read.
                            lease.discard();
                            return;
                        }
                    },
                    Ok(Err(error)) => {
                        let _ = tx.send(Err(Error::from_io(&path, error))).await;
                        lease.discard();
                        return;
                    },
                    Err(_) => {
                        let _ = tx.send(Err(Error::Timeout(path.clone()))).await;
                        lease.discard();
                        return;
                    },
                }
            }
            let finished = timeout(limit, reader.into_inner().finish()).await;
            match shared.settle(&path, &mut lease, finished) {
                Ok(()) => {
                    if let Some(batch) = buffer.drain() {
                        let _ = tx.send(Ok(batch)).await;
                    }
                },
                Err(error) => {
                    let _ = tx.send(Err(error)).await;
                },
            }
        });
        Ok(dir)
    }

    async fn stat(&self, path: &str) -> Result<Entry> {
        let path = self.shared.absolute(path)?;
        let mut lease = self.shared.lease().await?;
        if self.shared.has_mlst {
            let limit = self.shared.config.command_timeout;
            let result = timeout(limit, lease.ftp().mlst(Some(&path))).await;
            let line = self.shared.settle(&path, &mut lease, result)?;
            let file = ListParser::parse_mlst(&line)
                .map_err(|e| Error::Protocol(format!("{path}: bad MLST reply: {e}")))?;
            return Ok(Entry { name: base_name(&path), ..entry_from_list(&file) });
        }
        self.stat_with_size(&path, &mut lease).await
    }

    async fn mkdir(&self, path: &str) -> Result<()> {
        let path = self.shared.absolute(path)?;
        let mut lease = self.shared.lease().await?;
        let result = timeout(self.shared.config.command_timeout, lease.ftp().mkdir(&path)).await;
        self.shared.settle(&path, &mut lease, result)
    }

    async fn remove_file(&self, path: &str) -> Result<()> {
        let path = self.shared.absolute(path)?;
        let mut lease = self.shared.lease().await?;
        let result = timeout(self.shared.config.command_timeout, lease.ftp().rm(&path)).await;
        self.shared.settle(&path, &mut lease, result)
    }

    async fn remove_dir(&self, path: &str) -> Result<()> {
        let path = self.shared.absolute(path)?;
        let mut lease = self.shared.lease().await?;
        let result = timeout(self.shared.config.command_timeout, lease.ftp().rmdir(&path)).await;
        self.shared.settle(&path, &mut lease, result)
    }

    async fn rename(&self, from: &str, to: &str) -> Result<()> {
        let (from, to) = (self.shared.absolute(from)?, self.shared.absolute(to)?);
        let limit = self.shared.config.command_timeout;
        let mut lease = self.shared.lease().await?;
        let result = timeout(limit, lease.ftp().rename(&from, &to)).await;
        match self.shared.settle(&from, &mut lease, result) {
            Err(error) if !error.is_transient() => {
                // Most servers replace the target; for those that refuse, make room and try again.
                let removed = timeout(limit, lease.ftp().rm(&to)).await;
                if self.shared.settle(&to, &mut lease, removed).is_err() {
                    return Err(error);
                }
                let renamed = timeout(limit, lease.ftp().rename(&from, &to)).await;
                self.shared.settle(&from, &mut lease, renamed)
            },
            other => other,
        }
    }

    async fn read_range(
        &self,
        path: &str,
        offset: u64,
        len: Option<u64>,
    ) -> Result<Box<dyn RangeRead>> {
        let path = self.shared.absolute(path)?;
        let limit = self.shared.config.command_timeout;
        let mut lease = self.shared.lease().await?;
        if offset > 0 {
            let result = timeout(limit, lease.ftp().resume_transfer(offset as usize)).await;
            self.shared.settle(&path, &mut lease, result)?;
        }
        let result = timeout(limit, lease.ftp().retr_as_stream(&path)).await;
        let stream = self.shared.settle(&path, &mut lease, result)?;
        Ok(Box::new(FtpReader {
            stream: Some(stream),
            lease,
            shared: Arc::clone(&self.shared),
            path,
            remaining: len,
            finished: false,
        }))
    }

    async fn write_range(&self, path: &str, start: WriteStart) -> Result<Box<dyn RangeWrite>> {
        let path = self.shared.absolute(path)?;
        let limit = self.shared.config.command_timeout;
        let mut lease = self.shared.lease().await?;
        let result = match start {
            WriteStart::Truncate => timeout(limit, lease.ftp().put_with_stream(&path)).await,
            WriteStart::At(offset) => {
                let size = match timeout(limit, lease.ftp().size(&path)).await {
                    Ok(Ok(size)) => size as u64,
                    Ok(Err(error)) if keeps_connection(&error) => 0,
                    Ok(Err(error)) => {
                        lease.discard();
                        return Err(map_error(&path, error));
                    },
                    Err(_) => {
                        lease.discard();
                        return Err(Error::Timeout(path));
                    },
                };
                if size != offset {
                    return Err(Error::Unsupported(format!(
                        "{path}: FTP can only append at the end of the file ({size}), not at \
                         {offset}"
                    )));
                }
                timeout(limit, lease.ftp().append_with_stream(&path)).await
            },
        };
        let stream = self.shared.settle(&path, &mut lease, result)?;
        Ok(Box::new(FtpWriter {
            stream: Some(stream),
            lease,
            shared: Arc::clone(&self.shared),
            path,
        }))
    }
}

type FtpTransfer = TransferStream<suppaftp::tokio::AsyncRustlsStream>;

struct FtpReader {
    stream: Option<FtpTransfer>,
    lease: Lease,
    shared: Arc<Shared>,
    path: String,
    remaining: Option<u64>,
    finished: bool,
}

impl FtpReader {
    /// Ends the transfer at end of file, reading the server's completion reply.
    async fn end_of_file(&mut self) -> Result<()> {
        self.finished = true;
        let Some(stream) = self.stream.take() else { return Ok(()) };
        let result = timeout(self.shared.config.data_timeout, stream.finish()).await;
        self.shared.settle(&self.path, &mut self.lease, result)
    }
}

impl Drop for FtpReader {
    fn drop(&mut self) {
        // The server is still sending, so the connection has no usable state left.
        if !self.finished {
            self.lease.discard();
        }
    }
}

#[async_trait]
impl RangeRead for FtpReader {
    async fn next_chunk(&mut self) -> Result<Option<Bytes>> {
        if self.finished {
            return Ok(None);
        }
        if self.remaining == Some(0) {
            // Stopped short of the end of the file, so `Drop` closes the connection.
            return Ok(None);
        }
        let want = self.remaining.map_or(CHUNK, |left| left.min(CHUNK as u64) as usize);
        let mut buffer = BytesMut::with_capacity(want);
        let stream = self.stream.as_mut().expect("stream is present until finished");
        let read =
            match timeout(self.shared.config.data_timeout, stream.read_buf(&mut buffer)).await {
                Ok(Ok(read)) => read,
                Ok(Err(error)) => return Err(Error::from_io(&self.path, error)),
                Err(_) => return Err(Error::Timeout(self.path.clone())),
            };
        if read == 0 {
            self.end_of_file().await?;
            return Ok(None);
        }
        if let Some(remaining) = &mut self.remaining {
            *remaining -= read as u64;
        }
        Ok(Some(buffer.freeze()))
    }
}

struct FtpWriter {
    stream: Option<FtpTransfer>,
    lease: Lease,
    shared: Arc<Shared>,
    path: String,
}

impl Drop for FtpWriter {
    fn drop(&mut self) {
        if self.stream.is_some() {
            self.lease.discard();
        }
    }
}

#[async_trait]
impl RangeWrite for FtpWriter {
    async fn write(&mut self, chunk: Bytes) -> Result<()> {
        let stream = self.stream.as_mut().ok_or_else(|| Error::Io("writer is closed".into()))?;
        match timeout(self.shared.config.data_timeout, stream.write_all(&chunk)).await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(error)) => Err(Error::from_io(&self.path, error)),
            Err(_) => Err(Error::Timeout(self.path.clone())),
        }
    }

    async fn finish(mut self: Box<Self>) -> Result<()> {
        let Some(stream) = self.stream.take() else { return Ok(()) };
        let result = timeout(self.shared.config.data_timeout, stream.finish()).await;
        let path = self.path.clone();
        self.shared.settle(&path, &mut self.lease, result)
    }
}

#[cfg(test)]
mod tests {
    use tokio::io::{AsyncReadExt, AsyncWriteExt, duplex};

    use super::*;

    #[tokio::test]
    async fn tunnel_bridge_carries_bytes_both_ways_and_closes() {
        let (near, mut far) = duplex(4096);
        let mut tcp = into_tcp(DialedStream::Tunnel(Box::new(near))).await.unwrap();
        tcp.write_all(b"ping").await.unwrap();
        let mut got = [0u8; 4];
        far.read_exact(&mut got).await.unwrap();
        assert_eq!(&got, b"ping");
        far.write_all(b"pong").await.unwrap();
        tcp.read_exact(&mut got).await.unwrap();
        assert_eq!(&got, b"pong");
        drop(far);
        assert_eq!(tcp.read(&mut got).await.unwrap(), 0);
    }

    #[test]
    fn rejects_paths_that_could_inject_commands() {
        assert!(matches!(check_path("/a\r\nDELE b"), Err(Error::InvalidPath(_))));
        assert!(check_path("/plain name.txt").is_ok());
    }

    #[test]
    fn parses_posix_list_lines() {
        let file =
            parse_list_line("-rw-r--r-- 1 ftp ftp 1234 Nov 05 13:46 a b.txt", false).unwrap();
        assert_eq!(file.name(), "a b.txt");
        assert_eq!(file.size(), 1234);
        assert!(parse_list_line("total 12", false).is_none());
    }
}
