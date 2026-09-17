//! Inbound frame dispatch: the base-band opcode match, the forward to
//! the room's action channel, and the two encode/decode helpers every
//! arm shares.

use std::time::Instant;

use tokio::sync::mpsc;
use tracing::{debug, warn};

use gsb_protocol::op;
use gsb_protocol::{FrameBody, ProtoError, base};

use crate::conn::*;
use crate::registry::RegistryMsg;
use crate::room::Action;

impl super::ConnectionActor {
    pub(super) async fn handle_frame(&mut self, frame: FrameBody) {
        match frame.op {
            op::base::AUTH_REQ => self.handle_auth(frame).await,
            op::base::JOIN_ROOM_REQ => self.handle_join(frame).await,
            op::base::LEAVE_ROOM_REQ => {
                let ConnState::InRoom { room } = self.state else {
                    self.reply_err(ProtoError::NotInRoom).await;
                    return;
                };
                self.detach();
                let _ = self
                    .registry
                    .send(RegistryMsg::DespawnPlayer { conn: self.conn })
                    .await;
                let _ = self
                    .send_frame(op::base::LEAVE_ROOM_RESULT, &base::LeaveRoomResult {})
                    .await;
                debug!(%self.conn, room = %room, "left room");
            }
            op::base::HEARTBEAT => {
                let hb: base::Heartbeat = match self.decode::<base::Heartbeat>(frame.op, frame) {
                    Ok(m) => m,
                    Err(e) => {
                        self.reply_err(e).await;
                        return;
                    }
                };
                // Pre-auth heartbeat throttle (§3.2): at most one ACK per
                // second before auth success — the 1:1 request/response
                // amplification of unauthenticated liveness probing ends
                // here. Surplus heartbeats are counted in a dedicated
                // counter and get NO answer, deliberately NOT budgeted
                // (see the field doc: the throttle already caps the cost,
                // so scoring them would only punish honest-buggy clients).
                // Post-auth the branch below is byte-identical to the
                // pre-Tur-B behavior: every heartbeat answered (the
                // session's liveness signal).
                if self.state == ConnState::WaitingAuth {
                    let now = Instant::now();
                    let due = self
                        .last_preauth_hb_ack
                        .is_none_or(|t| now.duration_since(t) >= PREAUTH_HEARTBEAT_MIN_INTERVAL);
                    if !due {
                        self.m_preauth_hb_extra += 1;
                        debug!(
                            %self.conn,
                            extra = self.m_preauth_hb_extra,
                            "pre-auth heartbeat over the 1/s answer rate; counted, not answered"
                        );
                        return;
                    }
                    self.last_preauth_hb_ack = Some(now);
                }
                let _ = self
                    .send_frame(
                        op::base::HEARTBEAT_ACK,
                        &base::HeartbeatAck { tick: hb.tick },
                    )
                    .await;
            }
            // Correlated request (the RPC pattern, see `crate::rpc`): a
            // room-scoped operation, so it is forwarded to the room like
            // a game-band op (the room's core decodes the base envelope
            // and owns the correlation: pending caps, timeouts, the
            // exactly-one-answer reconciliation). The actor stays thin —
            // it does not decode the envelope, so a malformed envelope is
            // a normal rejection answered by the room (the same class as
            // an undecodable game payload), never a base-band violation
            // here. Not in a room → the same `NotInRoom` race-class
            // answer as any other game op (a request racing a leave or a
            // destroyed room is a legitimate ~1-RTT stray).
            op::base::RPC_REQ => {
                self.forward_to_room(frame).await;
            }
            // Unknown *base-band* opcode: no legitimate client sends one,
            // so it is a hard protocol violation (answered + budgeted).
            // Note this is what *makes* `UnknownOpcode` reachable in the
            // funnel: the actor only decodes its control ops, so before
            // this check an unknown base-band opcode would have been
            // forwarded as an (ignored) room action.
            unknown if unknown < gsb_protocol::op::GAME_BAND_START => {
                self.reply_err(ProtoError::UnknownOpcode(unknown)).await;
            }
            // Game band: the game crate owns these opcodes (the message
            // table is built per game); the room's ingest decides.
            _ => self.forward_to_room(frame).await,
        }
    }

    pub(super) async fn forward_to_room(&mut self, frame: FrameBody) {
        let mailbox = match &self.actions {
            Some(mb) => mb,
            None => {
                // Not in a room (or the room went away): report it.
                self.reply_err(ProtoError::NotInRoom).await;
                return;
            }
        };
        // The payload is forwarded encoded; the game crate decodes it.
        // Non-blocking: a flooding connection drops its own input (bounded
        // per-connection memory) and never stalls its actor or the room —
        // this `try_send` Full case is the architecture's only input-loss
        // point (the room's READ phase is a bounded pull that defers, not
        // drops), so the drop is counted here, attributed to this
        // connection's metrics sample. A closed channel means the room is
        // gone: detach.
        match mailbox.try_send(Action {
            conn: self.conn,
            // The connection actor cannot know the stable player identity
            // (it is minted inside the game logic): placeholder — the room
            // stamps the authoritative value from its binding table before
            // ingest (Faz 2).
            player: crate::id::PlayerId(0),
            op: frame.op,
            payload: frame.payload,
        }) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Full(_)) => {
                self.m_actions_dropped += 1;
                if !self.m_actions_dropped_warned {
                    self.m_actions_dropped_warned = true;
                    warn!(
                        %self.conn,
                        op = frame.op,
                        "action channel full; this connection's input is being \
                         dropped (counted in its metrics sample; the room's \
                         per-connection per-tick pull budget bounds the \
                         damage — another connection's input is never \
                         affected)"
                    );
                }
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                warn!(%self.conn, "action channel closed while forwarding; detaching");
                self.detach();
            }
        }
    }

    /// Decode a frame body into a concrete message type via the table.
    pub(super) fn decode<T: std::any::Any + Send + 'static>(
        &self,
        op: u16,
        frame: FrameBody,
    ) -> Result<T, ProtoError> {
        let decoded = self.table.decode(op, &frame.payload)?;
        decoded
            .downcast::<T>()
            .map(|b| *b)
            .map_err(|_| ProtoError::Other("type mismatch in message table".into()))
    }

    pub(super) async fn send_frame<M: prost::Message + std::any::Any>(&mut self, op: u16, msg: &M) {
        if let Some(fb) = self.table.frame(op, msg) {
            // Metrics: count this control frame's wire bytes (frame body:
            // 2-byte op + payload). Room fan-out bytes are counted by the
            // room, not here.
            self.m_out_bytes = self.m_out_bytes.saturating_add(2 + fb.payload.len() as u64);
            self.m_out_frames += 1;
            let _ = self.out.send(vec![fb]).await;
        }
    }
}
