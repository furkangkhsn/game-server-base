//! One connection to a gsb server, whatever carries it: a stream door
//! (TCP, TLS, a QUIC bi-stream — all the same length-prefixed frames) or
//! the rUDP client half of `gsb_net`. The one place the transports
//! differ, so that nothing above it has to.

use std::io;
use std::time::Duration;

use bytes::Bytes;
use gsb_net::udp::UdpClient;
use gsb_protocol::FrameBody;
use tokio::io::{AsyncRead, AsyncWrite};

use crate::frame::{FrameRx, FrameTx};

/// A type-erased stream read half (plaintext, TLS and QUIC alike).
pub type BoxRead = Box<dyn AsyncRead + Unpin + Send>;
/// A type-erased stream write half.
pub type BoxWrite = Box<dyn AsyncWrite + Unpin + Send>;

/// What one bounded receive found.
#[derive(Debug)]
pub enum Recv {
    /// The next frame.
    Frame(FrameBody),
    /// The stream ended at a frame boundary (EOF). Never on rUDP: UDP
    /// has no FIN — a session's end there is silence (and, from a gsb
    /// server that closes it, the `ERROR` frame before it).
    Closed,
    /// Nothing within the window.
    Quiet,
}

/// A client connection.
pub enum Conn {
    /// Length-prefixed frames over a byte stream, split in halves.
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

    /// Whether this is rUDP — the one transport without EOF.
    pub fn is_udp(&self) -> bool {
        matches!(self, Self::Udp(_))
    }

    /// The rUDP client (its statistics, its liveness), when it is one.
    pub fn udp_client(&self) -> Option<&UdpClient> {
        match self {
            Self::Udp(c) => Some(c),
            Self::Stream { .. } => None,
        }
    }

    /// Send one frame (stream: written and flushed; rUDP: the control
    /// band for base opcodes, the lossy band for the game band — the
    /// client half's own split).
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
    /// refused frame (over the size guard, too short) is `Err`; so is an
    /// rUDP socket error.
    pub async fn recv(&mut self, window: Duration) -> io::Result<Recv> {
        match self {
            Self::Stream { rx, .. } => match tokio::time::timeout(window, rx.next()).await {
                Ok(Ok(Some(f))) => Ok(Recv::Frame(f)),
                Ok(Ok(None)) => Ok(Recv::Closed),
                Ok(Err(e)) => Err(e),
                Err(_) => Ok(Recv::Quiet),
            },
            Self::Udp(c) => Ok(match c.recv_frame(window).await? {
                Some(f) => Recv::Frame(f),
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
