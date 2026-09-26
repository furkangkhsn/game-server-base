//! The write half: encoded frames onto a byte sink — the stream wire, or
//! one masked WebSocket message per frame.

use std::io;

use gsb_protocol::FrameBody;
use tokio::io::{AsyncWrite, AsyncWriteExt};

use super::{encode, encode_into};
use crate::ws::{OP_BIN, WsWrite};

/// The write half: encoded frames onto a byte sink — the stream wire
/// itself, or one masked WebSocket message per frame ([`crate::ws`]).
pub struct FrameTx<W> {
    inner: W,
    ws: Option<Box<WsWrite>>,
}

impl<W: AsyncWrite + Unpin> FrameTx<W> {
    pub fn new(sink: W) -> Self {
        Self {
            inner: sink,
            ws: None,
        }
    }

    /// The WebSocket writer ([`crate::ws::handshake`] builds it).
    pub(crate) fn ws(sink: W, ws: WsWrite) -> Self {
        Self {
            inner: sink,
            ws: Some(Box::new(ws)),
        }
    }

    /// Write one frame and flush it.
    pub async fn send(&mut self, op: u16, payload: &[u8]) -> io::Result<()> {
        self.feed(op, payload).await?;
        self.inner.flush().await
    }

    /// Write several frames in ONE write, then flush — e.g. AUTH and JOIN
    /// pipelined (the connection actor processes them in order). On
    /// WebSocket: one message per frame, still one write.
    pub async fn send_batch(&mut self, frames: &[FrameBody]) -> io::Result<()> {
        let mut out = Vec::new();
        for f in frames {
            match &mut self.ws {
                Some(ws) => ws.queue_data(OP_BIN, &encode(f.op, &f.payload))?,
                None => encode_into(&mut out, f.op, &f.payload),
            }
        }
        match &mut self.ws {
            Some(ws) => ws.drain(&mut self.inner).await?,
            None => self.inner.write_all(&out).await?,
        }
        self.inner.flush().await
    }

    /// Write one frame without flushing (a tight send loop that flushes
    /// on its own schedule, or never — a plain socket needs no flush).
    pub async fn feed(&mut self, op: u16, payload: &[u8]) -> io::Result<()> {
        match &mut self.ws {
            Some(ws) => {
                ws.queue_data(OP_BIN, &encode(op, payload))?;
                ws.drain(&mut self.inner).await
            }
            None => self.inner.write_all(&encode(op, payload)).await,
        }
    }

    /// Flush what was fed (on WebSocket, owed control replies first).
    pub async fn flush(&mut self) -> io::Result<()> {
        self.flush_control().await?;
        self.inner.flush().await
    }

    /// Send the control replies the WebSocket read half owes (pongs, the
    /// close echo); a no-op with nothing owed and on every other stream.
    pub(crate) async fn flush_control(&mut self) -> io::Result<()> {
        let Some(ws) = &mut self.ws else {
            return Ok(());
        };
        if ws.idle() {
            return Ok(());
        }
        ws.take_control()?;
        ws.drain(&mut self.inner).await?;
        self.inner.flush().await
    }

    /// Write ONE masked WebSocket frame of any kind, exactly as given
    /// (`fin`, `opcode`, `payload` — a text message, a fragment, a ping,
    /// a close with its code), and flush — for bytes outside the frame
    /// contract, like [`FrameTx::get_mut`] on a stream. `InvalidInput`
    /// when this is not a WebSocket.
    pub async fn ws_frame(&mut self, fin: bool, opcode: u8, payload: &[u8]) -> io::Result<()> {
        let Some(ws) = &mut self.ws else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "not a websocket connection",
            ));
        };
        ws.take_control()?;
        ws.queue(fin, opcode, payload)?;
        ws.drain(&mut self.inner).await?;
        self.inner.flush().await
    }

    /// The byte sink itself — for bytes outside the frame contract (a
    /// test writing a malformed prefix on purpose). On WebSocket this is
    /// the socket UNDER the WS framing: bytes written here skip it.
    pub fn get_mut(&mut self) -> &mut W {
        &mut self.inner
    }
}
