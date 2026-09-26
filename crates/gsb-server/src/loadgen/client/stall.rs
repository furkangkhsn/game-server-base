//! The slow reader (`--stall-ms`, the F11 measurement's load shape): a
//! client that stops reading for a while, on a small socket receive
//! buffer, so the server's writer blocks and the fan-out's bounded
//! channel drops the client's batches.

use std::net::SocketAddr;
use std::time::Duration;

use tokio::net::TcpStream;

/// The slow reader (`--stall-ms`): the client stops reading for `pause`
/// once every `every`, phase-staggered by id, on a TCP receive buffer of
/// [`STALL_RCVBUF`] — the server's writer blocks on the socket, and the
/// fan-out drops the batches its bounded channel cannot hold.
#[derive(Clone, Copy)]
pub(crate) struct Stall {
    pub(crate) pause: Duration,
    pub(crate) every: Duration,
}

impl Stall {
    /// How long client `id`, `since` into its session, is to stay away
    /// from its socket now (`None`: it reads).
    pub(crate) fn pause_left(&self, id: u64, since: Duration) -> Option<Duration> {
        let every = self.every.as_millis().max(1) as u64;
        let at = (since.as_millis() as u64).wrapping_add(id.wrapping_mul(997)) % every;
        let pause = self.pause.as_millis() as u64;
        (at < pause).then(|| Duration::from_millis(pause - at))
    }
}

/// A stalling TCP client's socket receive buffer (the kernel doubles it).
pub(crate) const STALL_RCVBUF: u32 = 16 * 1024;

/// A TCP connection, with the socket's receive buffer set first when
/// `rcvbuf` asks (the slow reader, [`Stall`]) — and its MSS clamped to
/// [`STALL_MSS`]: the peer sizes its send buffer by its congestion
/// window in segments, so on loopback (a 64 KiB MSS) a stalled reader
/// would sit behind megabytes of kernel buffer before the server's
/// writer ever blocked.
pub(super) async fn tcp_connect(
    addr: SocketAddr,
    rcvbuf: Option<u32>,
) -> std::io::Result<TcpStream> {
    let Some(n) = rcvbuf else {
        return TcpStream::connect(addr).await;
    };
    let socket = socket2::Socket::new(
        socket2::Domain::for_address(addr),
        socket2::Type::STREAM,
        Some(socket2::Protocol::TCP),
    )?;
    socket.set_recv_buffer_size(n as usize)?;
    socket.set_tcp_mss(STALL_MSS)?;
    socket.set_nonblocking(true)?;
    tokio::net::TcpSocket::from_std_stream(socket.into())
        .connect(addr)
        .await
}

/// A stalling TCP client's MSS (see [`tcp_connect`]).
const STALL_MSS: u32 = 1200;

#[cfg(test)]
mod tests {
    use super::*;

    /// A 1 s pause every 5 s, staggered by id: in the pause the client
    /// is told how long is left; outside it, nothing — and two ids are
    /// out of phase.
    #[test]
    fn the_pause_is_a_window_of_each_period() {
        let s = Stall {
            pause: Duration::from_millis(1000),
            every: Duration::from_millis(5000),
        };
        let ms = Duration::from_millis;
        assert_eq!(s.pause_left(0, ms(0)), Some(ms(1000)));
        assert_eq!(s.pause_left(0, ms(400)), Some(ms(600)));
        assert_eq!(s.pause_left(0, ms(1000)), None);
        assert_eq!(s.pause_left(0, ms(4999)), None);
        assert_eq!(s.pause_left(0, ms(5250)), Some(ms(750)));
        // id 1 is 997 ms ahead in its cycle.
        assert_eq!(s.pause_left(1, ms(0)), Some(ms(3)));
        assert_eq!(s.pause_left(1, ms(3)), None);
    }
}
