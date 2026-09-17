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
                // Echo the peer's status code (empty close echoes empty).
                let echo = match frame.payload.as_slice() {
                    [] => Vec::new(),
                    [hi] => {
                        return Err(self.proto_fail(
                            1002,
                            format_args!(
                                "close payload must be empty or 2 bytes, got 1 ({hi:#04x})"
                            ),
                        ));
                    }
                    [hi, lo, ..] => vec![*hi, *lo],
                };
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
