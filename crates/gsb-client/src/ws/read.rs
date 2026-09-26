//! The read half: server WS frames in, gsb frames out. All state lives
//! in the struct (the buffer, the open fragmented message, the close),
//! and the only await is a buffered read — so a read cancelled at any
//! point loses nothing.

use std::io;

use bytes::{Buf, Bytes, BytesMut};
use gsb_protocol::FrameBody;
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::sync::mpsc;

use super::{Control, MAX_CONTROL_PAYLOAD, OP_BIN, OP_CLOSE, OP_CONT, OP_PING, OP_PONG, WsClose};

/// Spare room kept in the buffer for each read.
const READ_CHUNK: usize = 8 * 1024;

/// What one read found, one level below [`crate::frame::FrameRx::next`].
pub(crate) enum Event {
    /// A gsb frame.
    Frame(FrameBody),
    /// A control frame that queued a reply (a ping): the caller holding
    /// the write half sends it now.
    Control,
    /// The session is over (a close frame, or EOF at a message boundary).
    End,
}

/// What one parse step made of the buffered bytes.
enum Step {
    NeedData,
    Continue,
    Event(Event),
}

pub(crate) struct WsRead<R> {
    io: R,
    buf: BytesMut,
    /// The largest message: the envelope's 4-byte prefix plus the body
    /// the frame guard allows.
    max_message: usize,
    /// The binary message being reassembled, when one is open.
    frag: Option<BytesMut>,
    ctrl: mpsc::Sender<Control>,
    /// The server's close frame, once read.
    close: Option<WsClose>,
    /// The TCP end was read: every later read reports the end.
    eof: bool,
    /// Set by a protocol error: the stream position is lost for good.
    failed: bool,
}

fn bad(why: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, why.into())
}

impl<R: AsyncRead + Unpin> WsRead<R> {
    pub(crate) fn new(
        io: R,
        leftover: BytesMut,
        max_frame_bytes: usize,
        ctrl: mpsc::Sender<Control>,
    ) -> Self {
        Self {
            io,
            buf: leftover,
            max_message: max_frame_bytes.saturating_add(4),
            frag: None,
            ctrl,
            close: None,
            eof: false,
            failed: false,
        }
    }

    /// The close frame that ended the session, if one did.
    pub(crate) fn close(&self) -> Option<&WsClose> {
        self.close.as_ref()
    }

    /// The next event. Cancel-safe (module docs).
    ///
    /// After the close frame the reader waits for the TCP end (§7.1.1:
    /// the server closes it): EOF is the end again, and a byte after the
    /// close is a violation — nothing may follow it on the wire.
    pub(crate) async fn next_event(&mut self) -> io::Result<Event> {
        loop {
            if self.eof {
                return Ok(Event::End);
            }
            if self.failed {
                return Err(bad("the websocket failed earlier"));
            }
            let step = if self.close.is_some() && !self.buf.is_empty() {
                Err(bad("bytes after the close frame"))
            } else if self.close.is_some() {
                Ok(Step::NeedData)
            } else {
                self.step()
            };
            match step {
                Ok(Step::Event(e)) => return Ok(e),
                Ok(Step::Continue) => continue,
                Ok(Step::NeedData) => {}
                Err(e) => {
                    self.failed = true;
                    return Err(e);
                }
            }
            self.buf.reserve(READ_CHUNK);
            if self.io.read_buf(&mut self.buf).await? == 0 {
                let boundary = self.buf.is_empty() && self.frag.is_none();
                if self.close.is_some() || boundary {
                    self.eof = true;
                    return Ok(Event::End);
                }
                self.failed = true;
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "the stream ended inside a websocket message",
                ));
            }
        }
    }

    /// Take one frame off the front of the buffer, if it is complete.
    fn step(&mut self) -> io::Result<Step> {
        let Some((fin, opcode, head, len)) = self.header()? else {
            return Ok(Step::NeedData);
        };
        if self.buf.len() < head + len {
            return Ok(Step::NeedData);
        }
        self.buf.advance(head);
        let payload = self.buf.split_to(len);
        match opcode {
            OP_BIN if self.frag.is_some() => {
                Err(bad("a new message started inside an open fragmented one"))
            }
            OP_BIN if fin => self.deliver(payload.freeze()),
            OP_BIN => {
                self.frag = Some(payload);
                Ok(Step::Continue)
            }
            OP_CONT => {
                let Some(mut msg) = self.frag.take() else {
                    return Err(bad("a continuation frame with no message open"));
                };
                msg.extend_from_slice(&payload);
                if fin {
                    return self.deliver(msg.freeze());
                }
                self.frag = Some(msg);
                Ok(Step::Continue)
            }
            OP_PING => {
                // §5.5.3: the pong carries the ping's payload back.
                let _ = self.ctrl.try_send((OP_PONG, payload.to_vec()));
                Ok(Step::Event(Event::Control))
            }
            OP_PONG => Ok(Step::Continue),
            OP_CLOSE => self.closed(&payload),
            // Text included: the gsb wire contract has no text messages.
            other => Err(bad(format!(
                "websocket opcode {other:#x} is not part of the wire contract"
            ))),
        }
    }

    /// Decode the frame header at the front of the buffer:
    /// `(fin, opcode, header length, payload length)`. Every size rule is
    /// judged here, before the payload is ever waited for.
    fn header(&self) -> io::Result<Option<(bool, u8, usize, usize)>> {
        let b = &self.buf[..];
        if b.len() < 2 {
            return Ok(None);
        }
        let (fin, opcode) = (b[0] & 0x80 != 0, b[0] & 0x0f);
        if b[0] & 0x70 != 0 {
            return Err(bad("RSV bits set, but no extension was negotiated"));
        }
        // §5.1: a client MUST fail on a masked server frame.
        if b[1] & 0x80 != 0 {
            return Err(bad("the server masked a frame"));
        }
        let (head, len) = match b[1] & 0x7f {
            126 if b.len() < 4 => return Ok(None),
            126 => (4, u64::from(u16::from_be_bytes([b[2], b[3]]))),
            127 if b.len() < 10 => return Ok(None),
            127 => {
                let mut n = [0u8; 8];
                n.copy_from_slice(&b[2..10]);
                (10, u64::from_be_bytes(n))
            }
            n => (2, u64::from(n)),
        };
        if opcode & 0x8 != 0 && (!fin || len > MAX_CONTROL_PAYLOAD as u64) {
            return Err(bad("a control frame must be whole and at most 125 bytes"));
        }
        let open = self.frag.as_ref().map_or(0, |m| m.len()) as u64;
        if opcode & 0x8 == 0 && open.saturating_add(len) > self.max_message as u64 {
            return Err(bad(format!(
                "a websocket message of over {} bytes: over the frame guard",
                self.max_message
            )));
        }
        Ok(Some((fin, opcode, head, len as usize)))
    }

    /// One whole binary message: exactly one stream-wire frame.
    fn deliver(&mut self, msg: Bytes) -> io::Result<Step> {
        if msg.len() < 4 {
            return Err(bad("a binary message shorter than the frame envelope"));
        }
        let declared = u32::from_le_bytes([msg[0], msg[1], msg[2], msg[3]]) as usize;
        if declared < 2 || msg.len() - 4 != declared {
            return Err(bad(format!(
                "the envelope declares {declared} body bytes in a {}-byte message: \
                 one frame per message is the contract",
                msg.len()
            )));
        }
        FrameBody::decode(msg.slice(4..))
            .map(|f| Step::Event(Event::Frame(f)))
            .map_err(|e| bad(e.to_string()))
    }

    /// The server's close: record it, queue the echo (§5.5.1: the code
    /// alone), end the session.
    fn closed(&mut self, payload: &[u8]) -> io::Result<Step> {
        let close = match payload {
            [] => WsClose {
                code: None,
                reason: String::new(),
            },
            [_] => return Err(bad("a 1-byte close payload has no status code")),
            [hi, lo, reason @ ..] => WsClose {
                code: Some(u16::from_be_bytes([*hi, *lo])),
                reason: String::from_utf8(reason.to_vec())
                    .map_err(|_| bad("the close reason is not UTF-8"))?,
            },
        };
        let echo = close.code.map(|c| c.to_be_bytes().to_vec());
        let _ = self.ctrl.try_send((OP_CLOSE, echo.unwrap_or_default()));
        self.close = Some(close);
        Ok(Step::Event(Event::End))
    }
}
