//! Message-level handling: fragmentation reassembly, the control
//! frames (close/ping/pong), and the mapping of one binary message
//! onto exactly one length-prefixed game frame.

use std::io;
use std::sync::atomic::Ordering;

use bytes::Bytes;

use gsb_protocol::FrameBody;

use super::Step;
use crate::ws::*;

impl super::WsReader {
    /// Map one assembled data message onto the wire contract: exactly one
    /// length-prefixed game frame per binary message.
    fn deliver(&mut self, msg: Bytes) -> io::Result<Step> {
        if msg.len() < 4 {
            return Err(self.proto_fail(
                1007,
                format_args!(
                    "binary message of {} bytes is shorter than the envelope",
                    msg.len()
                ),
            ));
        }
        let declared = u32::from_le_bytes([msg[0], msg[1], msg[2], msg[3]]) as usize;
        if declared < 2 {
            return Err(self.proto_fail(1007, "envelope body below the 2-byte minimum"));
        }
        if msg.len() != 4 + declared {
            return Err(self.proto_fail(
                1007,
                format_args!(
                    "envelope declares {declared} body bytes in a {}-byte message: \
                     exactly one game frame per message is the contract",
                    msg.len()
                ),
            ));
        }
        let frame = FrameBody::decode(msg.slice(4..))
            .map_err(|e| self.proto_fail(1007, format_args!("bad game frame body: {e}")))?;
        Ok(Step::Yield(frame))
    }

    /// Validate a client close payload and build its echo: the status code
    /// alone (an empty close echoes empty; the reason is not repeated).
    /// §5.5.1: one byte is no code at all; §7.4: only a sendable code may
    /// appear; §8.1: the reason must be UTF-8 (1007 otherwise).
    fn close_echo(&mut self, payload: &[u8]) -> io::Result<Vec<u8>> {
        let (code, reason) = match payload {
            [] => return Ok(Vec::new()),
            [hi, lo, reason @ ..] => (u16::from_be_bytes([*hi, *lo]), reason),
            [_] => return Err(self.proto_fail(1002, "a 1-byte close payload has no status code")),
        };
        if !close_code_is_sendable(code) {
            return Err(self.proto_fail(
                1002,
                format_args!("close code {code} may not appear on the wire"),
            ));
        }
        if std::str::from_utf8(reason).is_err() {
            return Err(self.proto_fail(1007, "close reason is not valid UTF-8"));
        }
        Ok(code.to_be_bytes().to_vec())
    }

    /// Consume the next complete frame (waiting is the caller's job via
    /// [`Step::NeedData`]) and update assembly/control state.
    pub(super) fn step(&mut self) -> io::Result<Step> {
        let Some(frame) = self.next_frame()? else {
            return Ok(Step::NeedData);
        };
        let control = matches!(frame.opcode, OP_CLOSE | OP_PING | OP_PONG);
        if control && (!frame.fin || frame.payload.len() > MAX_CONTROL_PAYLOAD) {
            return Err(self.proto_fail(
                1002,
                "control frames must be unfragmented and carry at most 125 bytes",
            ));
        }
        match frame.opcode {
            OP_CONT => {
                if self.frag_opcode.is_none() {
                    return Err(self.proto_fail(1002, "continuation frame with no message open"));
                }
                self.frag_data.extend_from_slice(&frame.payload);
                if self.frag_data.len() > self.max_message_bytes {
                    return Err(
                        self.proto_fail(1009, "reassembled message exceeds the size ceiling")
                    );
                }
                if frame.fin {
                    let data = std::mem::take(&mut self.frag_data);
                    self.frag_opcode = None;
                    self.deliver(data.freeze())
                } else {
                    Ok(Step::Continue)
                }
            }
            // §5.4: only CONTROL frames may interrupt a fragmented message.
            // A data frame here starts a second message inside the first —
            // a framing error, so 1002 even for text (the type question
            // below never arises).
            OP_TEXT | OP_BIN if self.frag_opcode.is_some() => Err(self.proto_fail(
                1002,
                "a new data message started inside an open fragmented one",
            )),
            // Text is rejected whether fragmented or not: the gsb wire
            // contract has no textual frames (module docs).
            OP_TEXT => {
                Err(self.proto_fail(1003, "text messages are not part of the wire contract"))
            }
            OP_BIN => {
                if frame.fin {
                    self.deliver(Bytes::from(frame.payload))
                } else {
                    self.frag_opcode = Some(OP_BIN);
                    self.frag_data.clear();
                    self.frag_data.extend_from_slice(&frame.payload);
                    Ok(Step::Continue)
                }
            }
            OP_CLOSE => {
                let echo = self.close_echo(&frame.payload)?;
                self.closing.store(true, Ordering::SeqCst);
                let _ = self.ctrl.try_send(WsOut::Control(OP_CLOSE, echo));
                // RFC 6455 §7.1.1: after echoing, the server closes first —
                // tell the writer task to drop the socket now instead of
                // waiting for the actor layer's teardown.
                let _ = self.ctrl.try_send(WsOut::Shutdown);
                Ok(Step::Done)
            }
            OP_PING => {
                // §5.5.3: pong carries the ping's application data back.
                let _ = self.ctrl.try_send(WsOut::Control(OP_PONG, frame.payload));
                Ok(Step::Continue)
            }
            OP_PONG => Ok(Step::Continue), // unsolicited pongs: ignore
            other => Err(self.proto_fail(1002, format_args!("unknown opcode {other:#04x}"))),
        }
    }
}

/// RFC 6455 §7.4 status codes a peer may put in a close frame: the
/// protocol's own 1000-1003 and 1007-1011, the IANA-registered 1012-1014
/// (service restart, try again later, bad gateway), and the library /
/// application ranges 3000-4999. Everything else is unused (0-999),
/// reserved (1004, 1016-2999), API-only and never sent (1005, 1006,
/// 1015), or undefined (5000+).
fn close_code_is_sendable(code: u16) -> bool {
    matches!(code, 1000..=1003 | 1007..=1014 | 3000..=4999)
}
