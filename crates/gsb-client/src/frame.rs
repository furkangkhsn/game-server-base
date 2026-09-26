//! The stream wire: `[u32 LE body length][u16 LE opcode][payload]` — the
//! shape every stream door (TCP, TLS, the QUIC bi-stream) speaks, and the
//! frame each WebSocket message and each rUDP datagram carries inside.
//!
//! The server side of the same contract is `gsb_net`'s framing adapter;
//! this is the client side, over any `AsyncRead` / `AsyncWrite` pair.
//!
//! [`FrameRx::next`] is cancel-safe: a partial frame stays buffered when
//! the future is dropped (a `timeout` around it), so a bounded read never
//! desyncs the stream. The hand-rolled `read_exact` readers this crate
//! replaced were not — a window that elapsed between the length prefix
//! and the body lost the prefix, and the next read parsed payload bytes
//! as a length.

use std::io;

use bytes::BytesMut;
use futures::StreamExt;
use gsb_protocol::FrameBody;
use tokio::io::AsyncRead;
use tokio_util::codec::{Decoder, FramedRead, LengthDelimitedCodec};

use crate::ws::{Event, WsClose, WsRead};

mod tx;

pub use tx::FrameTx;

/// The largest frame body a [`FrameRx`] accepts by default: 4 MiB, the
/// bound every client copy this crate replaced used. A longer declared
/// length is refused before its body is read (`InvalidData`).
pub const DEFAULT_MAX_FRAME_BYTES: usize = 4 * 1024 * 1024;

/// The bytes one frame occupies on a stream door: the 4-byte length
/// prefix, the 2-byte opcode and the payload.
pub fn wire_len(payload_len: usize) -> usize {
    4 + 2 + payload_len
}

/// Append one encoded frame to `out`.
pub fn encode_into(out: &mut Vec<u8>, op: u16, payload: &[u8]) {
    out.reserve(wire_len(payload.len()));
    out.extend_from_slice(&((2 + payload.len()) as u32).to_le_bytes());
    out.extend_from_slice(&op.to_le_bytes());
    out.extend_from_slice(payload);
}

/// One encoded frame.
pub fn encode(op: u16, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(wire_len(payload.len()));
    encode_into(&mut out, op, payload);
    out
}

/// The length-delimited codec, the gsb way (little-endian prefix, the
/// size guard), with one refinement: a stream that ends INSIDE a frame
/// is `UnexpectedEof` — told apart from a read error, and from the clean
/// end at a frame boundary.
struct Codec(LengthDelimitedCodec);

impl Decoder for Codec {
    type Item = BytesMut;
    type Error = io::Error;

    fn decode(&mut self, src: &mut BytesMut) -> io::Result<Option<BytesMut>> {
        self.0.decode(src)
    }

    fn decode_eof(&mut self, buf: &mut BytesMut) -> io::Result<Option<BytesMut>> {
        match self.decode(buf)? {
            None if !buf.is_empty() => Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "the stream ended inside a frame",
            )),
            done => Ok(done),
        }
    }
}

/// The read half: decoded frames off a byte source — the stream wire
/// itself, or WebSocket messages carrying it ([`crate::ws`]).
pub struct FrameRx<R> {
    inner: Source<R>,
}

enum Source<R> {
    Stream(FramedRead<R, Codec>),
    Ws(Box<WsRead<R>>),
}

impl<R: AsyncRead + Unpin> FrameRx<R> {
    /// A reader with the [`DEFAULT_MAX_FRAME_BYTES`] guard.
    pub fn new(source: R) -> Self {
        Self::with_max(source, DEFAULT_MAX_FRAME_BYTES)
    }

    /// A reader refusing any frame body longer than `max_frame_bytes`.
    pub fn with_max(source: R, max_frame_bytes: usize) -> Self {
        let codec = LengthDelimitedCodec::builder()
            .little_endian()
            .max_frame_length(max_frame_bytes)
            .new_codec();
        Self {
            inner: Source::Stream(FramedRead::new(source, Codec(codec))),
        }
    }

    /// The WebSocket reader ([`crate::ws::handshake`] builds it).
    pub(crate) fn ws(read: WsRead<R>) -> Self {
        Self {
            inner: Source::Ws(Box::new(read)),
        }
    }

    /// The next frame. `Ok(None)`: the stream ended at a frame boundary
    /// (EOF; on WebSocket also the server's close frame — see
    /// [`FrameRx::ws_close`]). `Err`: a read error; a stream that ended
    /// inside a frame (`UnexpectedEof`); a length over the guard, or a
    /// body too short to carry an opcode (`InvalidData`; on WebSocket
    /// also a protocol violation). Cancel-safe (see the module docs).
    pub async fn next(&mut self) -> io::Result<Option<FrameBody>> {
        loop {
            match self.next_event().await? {
                Event::Frame(f) => return Ok(Some(f)),
                Event::End => return Ok(None),
                // A ping: its pong is queued for the write half.
                Event::Control => {}
            }
        }
    }

    /// [`FrameRx::next`] one level down: a WebSocket ping surfaces as
    /// [`Event::Control`], so a caller holding the write half can send
    /// the pong at once ([`crate::Conn::recv`]). Cancel-safe.
    pub(crate) async fn next_event(&mut self) -> io::Result<Event> {
        match &mut self.inner {
            Source::Ws(ws) => ws.next_event().await,
            Source::Stream(s) => match s.next().await {
                None => Ok(Event::End),
                Some(Err(e)) => Err(e),
                Some(Ok(body)) => FrameBody::decode(body.freeze())
                    .map(Event::Frame)
                    .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e)),
            },
        }
    }

    /// Whether this reads a WebSocket.
    pub fn is_ws(&self) -> bool {
        matches!(self.inner, Source::Ws(_))
    }

    /// The close frame that ended a WebSocket session (`None` before it,
    /// after an end without one, and on every other transport).
    pub fn ws_close(&self) -> Option<&WsClose> {
        match &self.inner {
            Source::Ws(ws) => ws.close(),
            Source::Stream(_) => None,
        }
    }
}

#[cfg(test)]
mod tests;
