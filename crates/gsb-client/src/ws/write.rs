//! The write half's WS layer: every frame masked with a fresh OS-random
//! key (RFC 6455 §5.3), queued whole into `pending` before anything is
//! awaited, then drained by partial writes — so a cancelled write never
//! leaves half a frame on the wire, only bytes queued for the next one.

use std::io;

use bytes::{Buf, BytesMut};
use tokio::io::{AsyncWrite, AsyncWriteExt};
use tokio::sync::mpsc;

use super::{Control, OP_CLOSE};

pub(crate) struct WsWrite {
    /// Encoded frames not yet on the wire.
    pending: BytesMut,
    /// Replies the read half owes (pongs, the close echo).
    ctrl: mpsc::Receiver<Control>,
    /// A close frame went out: no data frame may follow (§5.5.1), and
    /// the close echo is not sent twice.
    close_sent: bool,
}

impl WsWrite {
    pub(crate) fn new(ctrl: mpsc::Receiver<Control>) -> Self {
        Self {
            pending: BytesMut::new(),
            ctrl,
            close_sent: false,
        }
    }

    /// Queue one data frame (the caller's gsb frame as a binary message).
    /// Owed replies go first — a close echo among them closes the door
    /// on this frame too.
    pub(crate) fn queue_data(&mut self, opcode: u8, body: &[u8]) -> io::Result<()> {
        self.take_control()?;
        if self.close_sent {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "the websocket is closed: no data frame may follow a close",
            ));
        }
        self.queue(true, opcode, body)
    }

    /// Move the read half's owed replies into `pending`.
    pub(crate) fn take_control(&mut self) -> io::Result<()> {
        while let Ok((opcode, payload)) = self.ctrl.try_recv() {
            if opcode == OP_CLOSE && self.close_sent {
                continue;
            }
            self.queue(true, opcode, &payload)?;
        }
        Ok(())
    }

    /// Whether anything waits to be written.
    pub(crate) fn idle(&self) -> bool {
        self.pending.is_empty() && self.ctrl.is_empty()
    }

    /// Queue one masked frame of any kind, as given.
    pub(crate) fn queue(&mut self, fin: bool, opcode: u8, payload: &[u8]) -> io::Result<()> {
        let mut key = [0u8; 4];
        getrandom::fill(&mut key).map_err(|e| io::Error::other(format!("OS entropy: {e}")))?;
        let out = &mut self.pending;
        out.reserve(14 + payload.len());
        out.extend_from_slice(&[if fin { 0x80 } else { 0 } | opcode]);
        let len = payload.len();
        if len < 126 {
            out.extend_from_slice(&[0x80 | len as u8]);
        } else if len <= usize::from(u16::MAX) {
            out.extend_from_slice(&[0x80 | 126]);
            out.extend_from_slice(&(len as u16).to_be_bytes());
        } else {
            out.extend_from_slice(&[0x80 | 127]);
            out.extend_from_slice(&(len as u64).to_be_bytes());
        }
        out.extend_from_slice(&key);
        out.extend(payload.iter().enumerate().map(|(i, b)| b ^ key[i & 3]));
        self.close_sent |= opcode == OP_CLOSE;
        Ok(())
    }

    /// Write everything queued. Cancel-safe: a byte leaves `pending` only
    /// once a write reported it taken.
    pub(crate) async fn drain<W: AsyncWrite + Unpin>(&mut self, io: &mut W) -> io::Result<()> {
        while !self.pending.is_empty() {
            let n = io.write(&self.pending).await?;
            if n == 0 {
                return Err(io::ErrorKind::WriteZero.into());
            }
            self.pending.advance(n);
        }
        Ok(())
    }
}
