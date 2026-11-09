use std::future::Future;
use std::net::SocketAddr;
use std::time::Duration;

use log::debug;
use russh::ChannelOpenFailure;
use tokio::io::{AsyncReadExt, AsyncWriteExt, copy_bidirectional};
use tokio::net::{TcpListener, TcpStream, ToSocketAddrs};
use tokio::sync::mpsc;
use tokio::task::{JoinHandle, JoinSet};

use crate::client::{Incoming, SshClient};
use crate::error::Error;
use crate::stream::SshStream;

/// How long a SOCKS client gets to say where it wants to go.
const SOCKS_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// Remote-forward connections waiting to be picked up.
const REMOTE_QUEUE: usize = 32;

/// A running port forward.
///
/// Dropping it stops the forward and cuts its active connections; for a remote
/// forward the listener is cancelled on the server in the background. Drop it
/// inside a Tokio runtime. [`close`](Self::close) does the same and waits for
/// the server. A forward keeps its connection alive.
#[derive(Debug)]
pub struct PortForward {
    local_addr: Option<SocketAddr>,
    remote_port: Option<u16>,
    task: JoinHandle<()>,
    cancel: Option<RemoteCancel>,
}

#[derive(Debug)]
struct RemoteCancel {
    client: SshClient,
    address: String,
    port: u32,
}

impl RemoteCancel {
    async fn run(self) -> Result<(), Error> {
        self.client.shared().routes.lock().await.remove(&(self.address.clone(), self.port));
        self.client
            .handle()
            .cancel_tcpip_forward(self.address.as_str(), self.port)
            .await
            .map_err(|e| self.client.classify(e))
    }
}

impl PortForward {
    /// The local listening address, for local and dynamic forwards.
    pub fn local_addr(&self) -> Option<SocketAddr> {
        self.local_addr
    }

    /// The port listening on the server, for remote forwards.
    pub fn remote_port(&self) -> Option<u16> {
        self.remote_port
    }

    /// Stops the forward and, for a remote forward, waits until the server has
    /// stopped listening.
    pub async fn close(mut self) -> Result<(), Error> {
        self.task.abort();
        match self.cancel.take() {
            Some(cancel) => cancel.run().await,
            None => Ok(()),
        }
    }
}

impl Drop for PortForward {
    fn drop(&mut self) {
        self.task.abort();
        if let Some(cancel) = self.cancel.take()
            && let Ok(runtime) = tokio::runtime::Handle::try_current()
        {
            runtime.spawn(async move {
                let _ = cancel.run().await;
            });
        }
    }
}

impl SshClient {
    /// Listens on `bind` and tunnels each connection to `target_host:target_port`
    /// as seen from the server (`ssh -L`). Use port 0 to let the system choose,
    /// then read [`PortForward::local_addr`].
    ///
    /// # Errors
    ///
    /// [`Error::Io`] if `bind` cannot be bound.
    pub async fn forward_local(
        &self,
        bind: impl ToSocketAddrs,
        target_host: &str,
        target_port: u16,
    ) -> Result<PortForward, Error> {
        let target_host = target_host.to_owned();
        let client = self.clone();
        self.listen(bind, move |tcp, peer| {
            let client = client.clone();
            let target_host = target_host.clone();
            async move {
                let origin = peer.ip().to_string();
                let opened = client
                    .open_direct_tcpip_from(
                        &target_host,
                        target_port,
                        &origin,
                        u32::from(peer.port()),
                    )
                    .await;
                match opened {
                    Ok(ssh) => splice(tcp, ssh).await,
                    Err(error) => debug!("local forward could not open a channel: {error}"),
                }
            }
        })
        .await
    }

    /// Runs a SOCKS5 proxy on `bind` that connects through the server
    /// (`ssh -D`). CONNECT only, and no client authentication, so bind a
    /// loopback address unless everyone who can reach it may use the tunnel.
    ///
    /// # Errors
    ///
    /// [`Error::Io`] if `bind` cannot be bound.
    pub async fn forward_dynamic(&self, bind: impl ToSocketAddrs) -> Result<PortForward, Error> {
        let client = self.clone();
        self.listen(bind, move |tcp, peer| serve_socks(client.clone(), tcp, peer)).await
    }

    async fn listen<F, Fut>(&self, bind: impl ToSocketAddrs, serve: F) -> Result<PortForward, Error>
    where
        F: Fn(TcpStream, SocketAddr) -> Fut + Send + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        let listener = TcpListener::bind(bind).await?;
        let local_addr = listener.local_addr()?;
        let client = self.clone();
        let task = tokio::spawn(accept_loop(listener, client, serve));
        Ok(PortForward { local_addr: Some(local_addr), remote_port: None, task, cancel: None })
    }

    /// Asks the server to listen on `bind_address:bind_port` and tunnels each
    /// connection back to `local_host:local_port` (`ssh -R`). Use port 0 to
    /// let the server choose, then read [`PortForward::remote_port`]. An empty
    /// or wildcard `bind_address` is subject to the server's `GatewayPorts`.
    ///
    /// # Errors
    ///
    /// [`Error::RequestDenied`] if the server refuses to listen.
    pub async fn forward_remote(
        &self,
        bind_address: &str,
        bind_port: u16,
        local_host: &str,
        local_port: u16,
    ) -> Result<PortForward, Error> {
        // Held across the request so that a connection arriving straight after
        // the server allocates a port waits for this route to exist.
        let mut routes = self.shared().routes.lock().await;
        let assigned =
            self.handle().tcpip_forward(bind_address, u32::from(bind_port)).await.map_err(
                |error| match self.classify(error) {
                    Error::RequestDenied(_) => Error::RequestDenied(format!(
                        "remote forward on {bind_address}:{bind_port}"
                    )),
                    other => other,
                },
            )?;
        let port = if bind_port == 0 { assigned } else { u32::from(bind_port) };

        let (route, incoming) = mpsc::channel(REMOTE_QUEUE);
        routes.insert((bind_address.to_owned(), port), route);
        drop(routes);

        let task = tokio::spawn(remote_dispatcher(
            incoming,
            self.clone(),
            local_host.to_owned(),
            local_port,
        ));
        Ok(PortForward {
            local_addr: None,
            remote_port: u16::try_from(port).ok(),
            task,
            cancel: Some(RemoteCancel {
                client: self.clone(),
                address: bind_address.to_owned(),
                port,
            }),
        })
    }
}

/// Accepts until the task is aborted or the connection ends. Connections live
/// in the `JoinSet`, so aborting this task cuts them too.
async fn accept_loop<F, Fut>(listener: TcpListener, client: SshClient, serve: F)
where
    F: Fn(TcpStream, SocketAddr) -> Fut + Send + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    let mut connections = JoinSet::new();
    loop {
        tokio::select! {
            accepted = listener.accept() => match accepted {
                Ok((tcp, peer)) => {
                    connections.spawn(serve(tcp, peer));
                }
                Err(error) => {
                    // Usually out of file descriptors, which passes.
                    debug!("accept failed: {error}");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            },
            Some(_) = connections.join_next(), if !connections.is_empty() => {}
            _ = client.closed() => break,
        }
    }
}

async fn remote_dispatcher(
    mut incoming: mpsc::Receiver<Incoming>,
    client: SshClient,
    local_host: String,
    local_port: u16,
) {
    let mut connections = JoinSet::new();
    loop {
        tokio::select! {
            connection = incoming.recv() => match connection {
                Some(connection) => {
                    connections.spawn(serve_remote(connection, local_host.clone(), local_port));
                }
                None => break,
            },
            Some(_) = connections.join_next(), if !connections.is_empty() => {}
            _ = client.closed() => break,
        }
    }
}

/// Connects locally first, so the server learns if the target is down.
async fn serve_remote(connection: Incoming, local_host: String, local_port: u16) {
    let Incoming { channel, reply } = connection;
    let mut tcp = match TcpStream::connect((local_host.as_str(), local_port)).await {
        Ok(tcp) => tcp,
        Err(error) => {
            debug!("remote forward could not reach its target: {error}");
            reply.reject(ChannelOpenFailure::ConnectFailed).await;
            return;
        },
    };
    reply.accept().await;
    let mut stream = channel.into_stream();
    if let Err(error) = copy_bidirectional(&mut tcp, &mut stream).await {
        debug!("remote forward connection ended: {error}");
    }
}

async fn splice(mut tcp: TcpStream, mut ssh: SshStream) {
    if let Err(error) = copy_bidirectional(&mut tcp, &mut ssh).await {
        debug!("forwarded connection ended: {error}");
    }
}

// SOCKS5 (RFC 1928) constants.
const SOCKS_VERSION: u8 = 5;
const NO_AUTH: u8 = 0;
const NO_ACCEPTABLE_METHOD: u8 = 0xff;
const CMD_CONNECT: u8 = 1;
const ATYP_IPV4: u8 = 1;
const ATYP_DOMAIN: u8 = 3;
const ATYP_IPV6: u8 = 4;
const REP_SUCCESS: u8 = 0;
const REP_GENERAL_FAILURE: u8 = 1;
const REP_NOT_ALLOWED: u8 = 2;
const REP_HOST_UNREACHABLE: u8 = 4;
const REP_REFUSED: u8 = 5;
const REP_COMMAND_NOT_SUPPORTED: u8 = 7;
const REP_ATYP_NOT_SUPPORTED: u8 = 8;

async fn serve_socks(client: SshClient, mut tcp: TcpStream, peer: SocketAddr) {
    let target = tokio::time::timeout(SOCKS_HANDSHAKE_TIMEOUT, read_socks_request(&mut tcp)).await;
    let (host, port) = match target {
        Ok(Ok(target)) => target,
        Ok(Err(error)) => {
            debug!("SOCKS handshake failed: {error}");
            return;
        },
        Err(_) => {
            debug!("SOCKS handshake timed out");
            return;
        },
    };
    let origin = peer.ip().to_string();
    match client.open_direct_tcpip_from(&host, port, &origin, u32::from(peer.port())).await {
        Ok(ssh) => {
            if send_socks_reply(&mut tcp, REP_SUCCESS).await.is_ok() {
                splice(tcp, ssh).await;
            }
        },
        Err(error) => {
            debug!("SOCKS connect failed: {error}");
            let _ = send_socks_reply(&mut tcp, socks_reply_for(&error)).await;
        },
    }
}

fn socks_reply_for(error: &Error) -> u8 {
    match error {
        Error::ChannelOpen { reason, .. } => match reason {
            ChannelOpenFailure::ConnectFailed => REP_REFUSED,
            ChannelOpenFailure::AdministrativelyProhibited => REP_NOT_ALLOWED,
            _ => REP_HOST_UNREACHABLE,
        },
        _ => REP_GENERAL_FAILURE,
    }
}

async fn send_socks_reply(tcp: &mut TcpStream, code: u8) -> std::io::Result<()> {
    tcp.write_all(&[SOCKS_VERSION, code, 0, ATYP_IPV4, 0, 0, 0, 0, 0, 0]).await
}

/// Runs the method negotiation and reads the CONNECT request, answering the
/// client on any refusal.
async fn read_socks_request(tcp: &mut TcpStream) -> std::io::Result<(String, u16)> {
    let mut greeting = [0u8; 2];
    tcp.read_exact(&mut greeting).await?;
    if greeting[0] != SOCKS_VERSION {
        return Err(std::io::Error::other("not a SOCKS5 client"));
    }
    let mut methods = vec![0u8; usize::from(greeting[1])];
    tcp.read_exact(&mut methods).await?;
    if !methods.contains(&NO_AUTH) {
        tcp.write_all(&[SOCKS_VERSION, NO_ACCEPTABLE_METHOD]).await?;
        return Err(std::io::Error::other("client requires authentication"));
    }
    tcp.write_all(&[SOCKS_VERSION, NO_AUTH]).await?;

    let mut header = [0u8; 4];
    tcp.read_exact(&mut header).await?;
    if header[0] != SOCKS_VERSION {
        return Err(std::io::Error::other("malformed SOCKS5 request"));
    }
    if header[1] != CMD_CONNECT {
        send_socks_reply(tcp, REP_COMMAND_NOT_SUPPORTED).await?;
        return Err(std::io::Error::other("only CONNECT is supported"));
    }
    let host = match header[3] {
        ATYP_IPV4 => {
            let mut octets = [0u8; 4];
            tcp.read_exact(&mut octets).await?;
            std::net::Ipv4Addr::from(octets).to_string()
        },
        ATYP_IPV6 => {
            let mut octets = [0u8; 16];
            tcp.read_exact(&mut octets).await?;
            std::net::Ipv6Addr::from(octets).to_string()
        },
        ATYP_DOMAIN => {
            let length = tcp.read_u8().await?;
            let mut name = vec![0u8; usize::from(length)];
            tcp.read_exact(&mut name).await?;
            match String::from_utf8(name) {
                Ok(name) => name,
                Err(_) => {
                    send_socks_reply(tcp, REP_ATYP_NOT_SUPPORTED).await?;
                    return Err(std::io::Error::other("host name is not UTF-8"));
                },
            }
        },
        _ => {
            send_socks_reply(tcp, REP_ATYP_NOT_SUPPORTED).await?;
            return Err(std::io::Error::other("unsupported address type"));
        },
    };
    let port = tcp.read_u16().await?;
    Ok((host, port))
}
