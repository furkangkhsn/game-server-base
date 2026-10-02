//! One connection to a gsb server, whatever carries it: a stream door
//! (TCP, TLS, a QUIC bi-stream — all the same length-prefixed frames; a
//! WebSocket — the same frames, one per message) or the rUDP client half
//! of `gsb_net`. The one place the transports
//! differ, so that nothing above it has to.

use std::io;
use std::time::Duration;

use bytes::Bytes;
use gsb_net::udp::UdpClient;
use gsb_protocol::FrameBody;
use tokio::io::{AsyncRead, AsyncWrite};

use crate::frame::{FrameRx, FrameTx};
use crate::ws::{Event, WsClose};

/// A type-erased stream read half (plaintext, TLS and QUIC alike).
pub type BoxRead = Box<dyn AsyncRead + Unpin + Send>;
/// A type-erased stream write half.
pub type BoxWrite = Box<dyn AsyncWrite + Unpin + Send>;

/// What one bounded receive found.
#[derive(Debug)]
pub enum Recv {
    /// The next frame.
    Frame(FrameBody),
    /// The session ended, after every frame received before its end was
    /// returned: a stream's EOF at a frame boundary (on WebSocket also
    /// the server's close frame, read back with [`Conn::ws_close`] — a
    /// unit variant, as every door's end is one); on rUDP, which has no
    /// FIN, the client's own verdict (B128) — its reliable band died, a
    /// stateless reset came, or a record-layer limit was crossed
    /// ([`gsb_net::udp::UdpClient::ended`] says which). Every read after
    /// it is `Closed` again, at once.
    Closed,
    /// Nothing within the window.
    Quiet,
}

/// A client connection.
pub enum Conn {
    /// Length-prefixed frames over a byte stream, split in halves (on
    /// WebSocket the halves speak RFC 6455 underneath: [`crate::ws`]).
    Stream {
        rx: FrameRx<BoxRead>,
        tx: FrameTx<BoxWrite>,
    },
    /// The rUDP client: one socket in one task (read and write share it,
    /// so there are no halves). Boxed: it carries a read buffer and its
    /// queues, far larger than the stream variant.
    Udp(Box<UdpClient>),
}

impl Conn {
    /// A stream connection over any byte stream (split into halves).
    pub fn stream<S>(io: S) -> Self
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        let (r, w) = tokio::io::split(io);
        Self::halves(Box::new(r), Box::new(w))
    }

    /// A stream connection over an already split pair (a QUIC bi-stream,
    /// a TCP stream's owned halves).
    pub fn halves(r: BoxRead, w: BoxWrite) -> Self {
        Self::Stream {
            rx: FrameRx::new(r),
            tx: FrameTx::new(w),
        }
    }

    /// The rUDP client as a connection.
    pub fn udp(client: UdpClient) -> Self {
        Self::Udp(Box::new(client))
    }

    /// Whether this is rUDP — the one transport without EOF (its end is
    /// the client's own verdict, still reported as [`Recv::Closed`]).
    pub fn is_udp(&self) -> bool {
        matches!(self, Self::Udp(_))
    }

    /// Whether this is a WebSocket.
    pub fn is_ws(&self) -> bool {
        matches!(self, Self::Stream { rx, .. } if rx.is_ws())
    }

    /// The close frame that ended a WebSocket session — its status code
    /// and reason — once [`Recv::Closed`] reported the end; `None` before
    /// it, for an end without a close frame, and on every other door.
    pub fn ws_close(&self) -> Option<&WsClose> {
        match self {
            Self::Stream { rx, .. } => rx.ws_close(),
            Self::Udp(_) => None,
        }
    }

    /// The rUDP client (its statistics, its liveness), when it is one.
    pub fn udp_client(&self) -> Option<&UdpClient> {
        match self {
            Self::Udp(c) => Some(c),
            Self::Stream { .. } => None,
        }
    }

    /// The rUDP client, mutably (its [`UdpClient::rebind`]: connection
    /// migration), when it is one.
    pub fn udp_client_mut(&mut self) -> Option<&mut UdpClient> {
        match self {
            Self::Udp(c) => Some(c),
            Self::Stream { .. } => None,
        }
    }

    /// Send one frame (stream: written and flushed; rUDP: the control
    /// band for base opcodes, the lossy band for the game band — the
    /// client half's own split). On an rUDP session that has ended it is
    /// `Err` (`NotConnected`), as a write after a stream's end fails.
    pub async fn send(&mut self, op: u16, payload: &[u8]) -> io::Result<()> {
        match self {
            Self::Stream { tx, .. } => tx.send(op, payload).await,
            Self::Udp(c) => c.send_frame(op, Bytes::copy_from_slice(payload)).await,
        }
    }

    /// Send several frames in order: one coalesced write on a stream,
    /// one datagram each on rUDP.
    pub async fn send_batch(&mut self, frames: &[FrameBody]) -> io::Result<()> {
        match self {
            Self::Stream { tx, .. } => tx.send_batch(frames).await,
            Self::Udp(c) => {
                for f in frames {
                    c.send_frame(f.op, f.payload.clone()).await?;
                }
                Ok(())
            }
        }
    }

    /// Wait up to `window` for the next frame. A stream's read error or
    /// refused frame (over the size guard, too short; a WebSocket
    /// protocol violation) is `Err`; so is an rUDP socket error. On
    /// WebSocket a ping read meanwhile is answered here, inside the
    /// window (cancel-safe: an unfinished pong stays queued). The end of
    /// the session is [`Recv::Closed`] on every door — on rUDP once the
    /// frames received before it are drained, and without waiting the
    /// window (`gsb_net::udp::UdpClient::recv_frame`).
    pub async fn recv(&mut self, window: Duration) -> io::Result<Recv> {
        match self {
            Self::Stream { rx, tx } => match tokio::time::timeout(window, next(rx, tx)).await {
                Ok(Ok(Some(f))) => Ok(Recv::Frame(f)),
                Ok(Ok(None)) => Ok(Recv::Closed),
                Ok(Err(e)) => Err(e),
                Err(_) => Ok(Recv::Quiet),
            },
            Self::Udp(c) => Ok(match c.recv_frame(window).await? {
                Some(f) => Recv::Frame(f),
                // Drained and over (B128): the rUDP end, as an EOF.
                None if !c.is_established() => Recv::Closed,
                None => Recv::Quiet,
            }),
        }
    }

    /// The stream halves, for a caller that reads and writes from two
    /// tasks (a reader loop and a mover). `Err` gives the connection back
    /// on rUDP, which has no halves.
    pub fn into_split(self) -> Result<(FrameRx<BoxRead>, FrameTx<BoxWrite>), Box<Self>> {
        match self {
            Self::Stream { rx, tx } => Ok((rx, tx)),
            udp @ Self::Udp(_) => Err(Box::new(udp)),
        }
    }
}

/// The next frame off a stream, sending what the WebSocket read half
/// owes on the way (a pong; the close echo at the end). A failed reply
/// write does not fail the read — the next send reports it.
async fn next(
    rx: &mut FrameRx<BoxRead>,
    tx: &mut FrameTx<BoxWrite>,
) -> io::Result<Option<FrameBody>> {
    loop {
        match rx.next_event().await? {
            Event::Frame(f) => return Ok(Some(f)),
            Event::Control => {
                let _ = tx.flush_control().await;
            }
            Event::End => {
                let _ = tx.flush_control().await;
                return Ok(None);
            }
        }
    }
}

#[cfg(test)]
mod tests;
