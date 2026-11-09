//! Terminal transports that are plain byte streams: Telnet and serial ports.
//!
//! Both give the pair [`rmx_terminal::Terminal::connect`] takes, from [`telnet::connect`] and
//! [`serial::open`]. Neither queues output: the next read of the stream happens when the terminal
//! driver asks, so a slow parser slows the remote end, and writes wait for room.

pub mod serial;
pub mod telnet;
