//! Binding a UDP door's socket with explicit kernel buffer sizes
//! (BACKLOG B4).
//!
//! A UDP door has no accept queue: ONE socket carries every session (the
//! rUDP demux, the QUIC endpoint), so its kernel **receive buffer** is
//! the first and only queue under a burst — a join storm, a room's input
//! fan-in — and a datagram that arrives while it is full is dropped by
//! the kernel before the server sees it (Linux counts it as
//! `Udp: RcvbufErrors` in `/proc/net/snmp`). The send buffer is the
//! same queue on the way out (`SndbufErrors`; a full one parks
//! `send_to` or fails `try_send_to`).
//!
//! Unset (the default), a size is **not touched**: no `setsockopt` is
//! made and the socket keeps the system default (Linux
//! `net.core.rmem_default` / `wmem_default`, 208 KiB on most
//! distributions) — the socket every UDP door had before the knob
//! existed.
//!
//! **The kernel's rule (Linux).** A requested size is capped at
//! `net.core.rmem_max` (receive) / `net.core.wmem_max` (send) — 208 KiB
//! by default on many distributions, so raising the buffer usually needs
//! the sysctl raised too — and then **doubled**: the kernel keeps the
//! second half for its own bookkeeping, so `getsockopt` reads back twice
//! the (capped) request. The usable space is therefore about the
//! request, up to the cap. A capped request is not an error (the bind
//! succeeds, as `setsockopt` does); the door logs a warning naming the
//! sysctl ([`log_buffers`]). Other systems keep their own rules (macOS
//! caps at `kern.ipc.maxsockbuf` and does not double).

use std::io;
use std::net::SocketAddr;

use socket2::{Domain, Protocol, SockRef, Socket, Type};
use tracing::{info, warn};

/// The smallest buffer a door may ask for: one page. Linux raises
/// anything smaller to its own floor (~2.3 KiB after doubling) without
/// saying so, and a buffer below a few datagrams is never what an
/// operator means — refused rather than silently rewritten.
pub const MIN_SOCKET_BUFFER: u32 = 4096;

/// The largest buffer a door may ask for: `setsockopt` takes a C `int`,
/// and a larger `u32` would wrap negative on the way there.
pub const MAX_SOCKET_BUFFER: u32 = i32::MAX as u32;

/// The kernel buffer sizes a UDP door asks for. `None` = leave the
/// system default alone (no `setsockopt` at all).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct UdpBuffers {
    /// `SO_RCVBUF`, in bytes.
    pub recv: Option<u32>,
    /// `SO_SNDBUF`, in bytes.
    pub send: Option<u32>,
}

/// Why `bytes` cannot be asked for as a socket buffer, or `None` when it
/// can (`MIN_SOCKET_BUFFER..=MAX_SOCKET_BUFFER`).
pub fn socket_buffer_problem(bytes: u32) -> Option<&'static str> {
    match bytes {
        n if n < MIN_SOCKET_BUFFER => Some("must be at least 4096 (one page)"),
        n if n > MAX_SOCKET_BUFFER => Some("must be at most 2147483647 (a C int)"),
        _ => None,
    }
}

/// Bind a UDP socket on `addr` with `buffers` applied before the bind
/// (so no datagram ever waits in a smaller queue), non-blocking — ready
/// for `tokio::net::UdpSocket::from_std` or a quinn runtime. Same socket
/// as `std::net::UdpSocket::bind` otherwise (the address's family,
/// close-on-exec). A size [`socket_buffer_problem`] names is an
/// `InvalidInput` error, before any socket exists.
pub fn bind_udp(addr: SocketAddr, buffers: UdpBuffers) -> io::Result<std::net::UdpSocket> {
    for (name, size) in [("receive", buffers.recv), ("send", buffers.send)] {
        if let Some(why) = size.and_then(socket_buffer_problem) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("UDP {name} buffer {} {why}", size.unwrap_or(0)),
            ));
        }
    }
    let socket = Socket::new(Domain::for_address(addr), Type::DGRAM, Some(Protocol::UDP))?;
    socket.set_nonblocking(true)?;
    if let Some(n) = buffers.recv {
        socket.set_recv_buffer_size(n as usize)?;
    }
    if let Some(n) = buffers.send {
        socket.set_send_buffer_size(n as usize)?;
    }
    socket.bind(&addr.into())?;
    Ok(socket.into())
}

/// The receive and send buffer sizes the kernel reports for `sock`
/// (`getsockopt`; on Linux twice the capped request — see the module
/// docs).
pub fn buffer_sizes<'s, S>(sock: &'s S) -> io::Result<(usize, usize)>
where
    SockRef<'s>: From<&'s S>,
{
    let s = SockRef::from(sock);
    Ok((s.recv_buffer_size()?, s.send_buffer_size()?))
}

/// Whether the kernel granted less usable space than `asked`: on Linux
/// the reported size is twice the capped request (see the module docs),
/// elsewhere it is the grant itself.
fn capped(asked: u32, reported: usize) -> bool {
    let usable = if cfg!(target_os = "linux") {
        reported / 2
    } else {
        reported
    };
    usable < asked as usize
}

/// Log the buffers a bound door got, and warn for each size the kernel
/// capped below the request (naming the sysctl that caps it). `door`
/// names the transport in the log line.
pub fn log_buffers(door: &str, addr: SocketAddr, asked: UdpBuffers, got: (usize, usize)) {
    let (recv, send) = got;
    info!(%addr, door, recv_buffer = recv, send_buffer = send,
          asked_recv = ?asked.recv, asked_send = ?asked.send,
          "UDP socket buffers (as the kernel reports them)");
    let checks = [
        (asked.recv, recv, "receive", "net.core.rmem_max"),
        (asked.send, send, "send", "net.core.wmem_max"),
    ];
    for (asked, reported, name, sysctl) in checks {
        if let Some(asked) = asked
            && capped(asked, reported)
        {
            warn!(%addr, door, asked, reported, sysctl,
                  "UDP {name} buffer capped by the kernel below the configured size \
                   (raise the sysctl to grant it)");
        }
    }
}

#[cfg(test)]
mod tests;
