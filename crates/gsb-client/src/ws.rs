//! WebSocket: the client half of `gsb_net::ws`, RFC 6455 over any byte
//! stream, producing the SAME [`Conn`] as every other door.
//!
//! The door's wire contract: every binary message carries exactly ONE
//! stream-wire frame `[u32 LE len][u16 LE op][payload]` — the bytes a
//! TCP door would carry, one frame per message. So a WS connection is a
//! [`Conn::Stream`] whose halves speak WS underneath: [`FrameRx`] parses
//! server frames (never masked), reassembles fragmented messages, checks
//! the one-frame envelope and answers control frames; [`FrameTx`] sends
//! each frame as one masked (RFC 6455 §5.3, a fresh OS-random key per
//! frame) FIN binary message. Nothing above the frame layer changes:
//! `send`, `recv`, the session steps and `into_split` work as on TCP.
//!
//! - A ping is answered with a pong carrying its payload. [`Conn::recv`]
//!   writes it at once; after [`Conn::into_split`] the read half queues
//!   it (bounded; a full queue drops it — RFC 6455 §5.5.3 lets a client
//!   answer only the latest) and the write half sends it ahead of its
//!   next frame.
//! - A close frame ends the session: [`Recv::Closed`](crate::Recv), with
//!   the status code and reason on [`Conn::ws_close`] (`code: None` =
//!   an empty close). The close is echoed (code only); no data frame may
//!   be sent after it. A later read waits for the server's TCP end
//!   (`Closed` again; `Quiet` while it lingers) — a byte after the close
//!   frame is refused. A TCP end with no close frame is `Closed` too,
//!   with `ws_close() == None`.
//! - Refused (`InvalidData`): a masked server frame, RSV bits, an unknown
//!   opcode, a text message (not in the contract), a fragmented or
//!   oversized control frame, a data frame inside an open fragmented
//!   message, an envelope that is not exactly one frame, and — from the
//!   frame HEADER, before its payload is awaited — a message longer than
//!   the frame guard allows (`4 + max_frame_bytes`, the envelope).
//! - Cancel-safe both ways: reads buffer what they took (never
//!   `read_exact` under a window); writes queue the whole encoded frame
//!   before the first await and drain it by partial writes, so a cancelled
//!   send leaves the rest queued for the next write, never half a frame.
//!
//! Plain `ws://`: [`connect::ws`](crate::connect::ws). [`handshake`] runs
//! over any stream the caller opened — a `wss://` endpoint is
//! [`handshake`] over a [`tls`](crate::tls) client stream. The gsb door
//! itself has no TLS form (a `"ws"` listener refuses TLS files), so that
//! composition is not exercised against a gsb server.

mod handshake;
mod read;
mod write;

#[cfg(test)]
mod tests;

use bytes::BytesMut;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::mpsc;

use crate::conn::{BoxRead, BoxWrite, Conn};
use crate::frame::{DEFAULT_MAX_FRAME_BYTES, FrameRx, FrameTx};

pub use handshake::accept_key;
pub(crate) use handshake::upgrade;
pub(crate) use read::{Event, WsRead};
pub(crate) use write::WsWrite;

/// Continuation frame (RFC 6455 §5.2 opcodes, for [`FrameTx::ws_frame`]).
pub const OP_CONT: u8 = 0x0;
/// Text frame — outside the gsb wire contract (the door answers 1003).
pub const OP_TEXT: u8 = 0x1;
/// Binary frame: the one carrying gsb frames.
pub const OP_BIN: u8 = 0x2;
/// Close frame.
pub const OP_CLOSE: u8 = 0x8;
/// Ping frame.
pub const OP_PING: u8 = 0x9;
/// Pong frame.
pub const OP_PONG: u8 = 0xA;

/// RFC 6455 §5.5: a control frame carries at most 125 payload bytes.
const MAX_CONTROL_PAYLOAD: usize = 125;

/// Control replies the read half owes the write half (pongs, the close
/// echo). Small: replies are best effort and only the latest ping needs
/// an answer.
const CONTROL_QUEUE: usize = 8;

/// A control reply queued by the read half: `(opcode, payload)`.
type Control = (u8, Vec<u8>);

/// The close frame that ended a WebSocket session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WsClose {
    /// The status code (RFC 6455 §7.4); `None` when the close was empty
    /// (what an API would report as 1005, "no status received").
    pub code: Option<u16>,
    /// The UTF-8 reason after the code (often empty).
    pub reason: String,
}

/// Run the RFC 6455 opening handshake over `io` (request `path`, `Host:
/// host`) and wrap the upgraded stream as a [`Conn`] with the default
/// frame guard. A refused upgrade, a wrong `Sec-WebSocket-Accept` or an
/// extension/subprotocol nobody asked for is an error; bytes the server
/// sent right behind its 101 are kept for the first read.
pub async fn handshake<S>(io: S, host: &str, path: &str) -> std::io::Result<Conn>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    handshake_with_max(io, host, path, DEFAULT_MAX_FRAME_BYTES).await
}

/// [`handshake`] with a frame guard of `max_frame_bytes` (the body the
/// envelope may declare, as [`FrameRx::with_max`]).
pub async fn handshake_with_max<S>(
    mut io: S,
    host: &str,
    path: &str,
    max_frame_bytes: usize,
) -> std::io::Result<Conn>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let leftover = handshake::upgrade(&mut io, host, path).await?;
    let (r, w) = tokio::io::split(io);
    Ok(conn(Box::new(r), Box::new(w), leftover, max_frame_bytes))
}

/// An upgraded stream's halves as a [`Conn`].
pub(crate) fn conn(r: BoxRead, w: BoxWrite, leftover: BytesMut, max_frame_bytes: usize) -> Conn {
    let (rx, tx) = halves(r, w, leftover, max_frame_bytes);
    Conn::Stream { rx, tx }
}

/// The WS reader and writer over already upgraded halves, wired by the
/// bounded control queue.
pub(crate) fn halves<R, W>(
    r: R,
    w: W,
    leftover: BytesMut,
    max_frame_bytes: usize,
) -> (FrameRx<R>, FrameTx<W>)
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let (ctrl_tx, ctrl_rx) = mpsc::channel(CONTROL_QUEUE);
    let rx = FrameRx::ws(WsRead::new(r, leftover, max_frame_bytes, ctrl_tx));
    let tx = FrameTx::ws(w, WsWrite::new(ctrl_rx));
    (rx, tx)
}
