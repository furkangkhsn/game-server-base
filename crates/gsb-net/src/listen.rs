//! Binding a TCP listening socket with an explicit accept backlog
//! (BACKLOG B84).
//!
//! `tokio::net::TcpListener::bind` passes a fixed backlog to `listen(2)`
//! — mio's, which since mio 1.1 is the standard library's 128 — so a
//! server could neither size its accept queue nor even know it without
//! reading its dependencies. Every TCP-based door (plain TCP, TLS,
//! WebSocket) and the ops HTTP surface bind through [`bind_tcp`]
//! instead; the default, [`DEFAULT_LISTEN_BACKLOG`], is exactly what
//! tokio passes today, so a server that does not ask keeps its accept
//! queue as it was.
//!
//! The kernel caps the value: the queue a socket really gets is
//! `min(backlog, somaxconn)` (Linux `net.core.somaxconn`, 4096 by
//! default since 5.4; macOS `kern.ipc.somaxconn`, 128 by default). A
//! larger value asks for more; it never fails because of the cap.
//!
//! UDP doors (rUDP, QUIC) have no accept queue: their counterpart under
//! a burst is the socket's receive buffer (`SO_RCVBUF`), a different
//! knob (BACKLOG B4).

use std::io;
use std::net::SocketAddr;

use tokio::net::{TcpListener, TcpSocket};

/// The accept backlog a listener gets when nothing asks for another:
/// 128, the value `tokio::net::TcpListener::bind` passes (mio ≥ 1.1,
/// matching the standard library) — the queue every door had before
/// the backlog became configurable. Named here so a dependency upgrade
/// cannot change it silently.
pub const DEFAULT_LISTEN_BACKLOG: u32 = 128;

/// The largest backlog [`bind_tcp`] takes: `listen(2)`'s argument is a C
/// `int`, and a larger `u32` would wrap negative on the way there.
pub const MAX_LISTEN_BACKLOG: u32 = i32::MAX as u32;

/// Why `backlog` cannot be passed to `listen(2)`, or `None` when it can
/// (`1..=`[`MAX_LISTEN_BACKLOG`]). Zero is refused rather than passed:
/// it does not mean "no queue" (Linux still queues one connection) and
/// kernels read it differently, so whoever wrote it meant something the
/// socket cannot say.
pub fn listen_backlog_problem(backlog: u32) -> Option<&'static str> {
    match backlog {
        0 => Some("must be at least 1"),
        n if n > MAX_LISTEN_BACKLOG => Some("must be at most 2147483647 (a C int)"),
        _ => None,
    }
}

/// Bind a TCP listener on `addr` with an accept queue of `backlog`
/// connections (see the module docs for the kernel's cap).
///
/// Same socket as `tokio::net::TcpListener::bind` otherwise: the
/// address's family, `SO_REUSEADDR` everywhere but Windows (there it
/// would let another socket take over a port in use), then `bind` and
/// `listen`. Must run inside a tokio runtime (the listener registers
/// with its reactor). A `backlog` [`listen_backlog_problem`] names is an
/// `InvalidInput` error, before any socket exists.
pub fn bind_tcp(addr: SocketAddr, backlog: u32) -> io::Result<TcpListener> {
    if let Some(why) = listen_backlog_problem(backlog) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("listen backlog {backlog} {why}"),
        ));
    }
    let socket = if addr.is_ipv4() {
        TcpSocket::new_v4()?
    } else {
        TcpSocket::new_v6()?
    };
    #[cfg(not(windows))]
    socket.set_reuseaddr(true)?;
    socket.bind(addr)?;
    socket.listen(backlog)
}

#[cfg(test)]
mod tests;
