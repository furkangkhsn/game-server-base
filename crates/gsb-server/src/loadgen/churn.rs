//! The reconnect-churn client: drop and resume on a cycle, so the
//! park/resume path is measured under load.
//!
//! Byte accounting is `run_client`'s: every frame written (AUTH, each
//! JOIN attempt, the inputs) and every frame read (the join phase's
//! replies included) at its real size on this wire — [`frame_bytes`]:
//! the length-prefixed frame on TCP/TLS, the datagram on rUDP, the
//! message on WebSocket.

use std::time::{Duration, Instant};

use gsb_client::session::{self, Credentials};
use gsb_client::{Conn, ServerError};
use gsb_protocol::base::{ErrorCode, JoinRoomResult};
use gsb_protocol::op;
use prost::Message;

use crate::client::*;

/// One auth+join round trip of a churn session, RETRYING the join on a
/// gentle rejection (ERROR code 4 — the stale-resume answer a fresh
/// connection can elicit after an earlier session already rebound the
/// park; the core's per-connection join-epoch counter grows only within
/// ONE connection, so the retry's higher epoch is accepted and the SAME
/// entity comes back). A real game client retries a transient join
/// failure; so do we, with a bounded budget.
pub(crate) async fn churn_join(
    id: u64,
    wire: &mut Conn,
    room: u64,
    rep: &mut ClientReport,
) -> Option<u64> {
    for _attempt in 0..5u32 {
        let join = session::join_req(room);
        // Every attempt is on the wire (the same convention as
        // `run_client`: counted when written).
        rep.bytes_out += frame_bytes(wire, Dir::Out, join.op, join.payload.len());
        if wire.send(join.op, &join.payload).await.is_err() {
            return None;
        }
        let mut retriable = false;
        let join_deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < join_deadline {
            let got = recv_wire(wire, Duration::from_millis(250)).await;
            // Every frame the join phase reads is inbound wire bytes —
            // the result and the refusals included.
            if let Got::Frame(op, payload) = &got {
                rep.bytes_in += frame_bytes(wire, Dir::In, *op, payload.len());
            }
            match got {
                Got::Frame(op::base::JOIN_ROOM_RESULT, payload) => {
                    return JoinRoomResult::decode(&payload[..]).ok().map(|m| m.entity);
                }
                Got::Frame(op::base::ERROR, payload) => {
                    // Classified through the GENERATED enum, not
                    // hand-copied numbers (an unknown code reads as
                    // `Unspecified`). The `_` arm implements the
                    // forward-compatibility rule from `base.proto` — an
                    // unknown or unspecified code counts as a plain
                    // error, never as one of the known decisions.
                    match ServerError::decode_lossy(&payload).code {
                        // The gentle stale-resume reject: retry with the
                        // same connection's next epoch.
                        ErrorCode::RoomOpFailed => {
                            if !retriable {
                                eprintln!(
                                    "churn client {id}: join answered 'stale \
                                     resume' (code 4); retrying on the same \
                                     connection (core join-epoch quirk)"
                                );
                            }
                            retriable = true;
                            break;
                        }
                        ErrorCode::RoomFull => rep.join_rejected += 1,
                        ErrorCode::ServerClosed => rep.cap_rejected += 1,
                        _ => rep.errors += 1,
                    }
                }
                Got::Frame(..) | Got::Quiet => {}
                Got::Dead => return None,
            }
        }
        if !retriable {
            return None;
        }
    }
    None
}

pub(crate) async fn run_churn_client(
    id: u64,
    p: ClientParams,
    cycle: Duration,
    max_drops: u64,
) -> ClientReport {
    let mut rep = ClientReport {
        id,
        connected: false,
        connect_ms: 0,
        joined: false,
        entity: 0,
        left: false,
        snapshots: 0,
        bytes_in: 0,
        bytes_out: 0,
        moves: 0,
        errors: 0,
        join_rejected: 0,
        cap_rejected: 0,
        budget_rejected: 0,
        retrans_out: 0,
        dup_in: 0,
        oob_dropped: 0,
        gave_up: 0,
        frag_reassembled: 0,
        frag_dropped: 0,
        hs_retries: 0,
        seq_first: None,
        seq_last: None,
        acks: 0,
        ack_processed_max: 0,
        ack_lag_max_ms: 0,
        fulls: 0,
        private_fulls: 0,
        deltas: 0,
        gap_drops: 0,
        view_size: 0,
        churn_cycles: 0,
        resumed: 0,
        fresh_joins: 0,
    };
    // ONE identity for every session of this client (the resume key):
    // this is what makes the reconnects RESUMES instead of fresh joins.
    let creds = Credentials::named(crate::bot::bot_name(id));
    // The wire id of the previous session (0 before the first join): the
    // continuity check that classifies each join as resume / fresh.
    let mut prev_entity: u64 = 0;
    // Completed DROP transitions so far (`max_drops` reached ⇒ the last
    // session lingers instead of dropping).
    let mut drops: u64 = 0;
    while Instant::now() < p.deadline {
        let cycle_end = (Instant::now() + cycle).min(p.deadline);
        let final_session = max_drops != 0 && drops >= max_drops;
        rep.churn_cycles += 1;

        // -- connect ───────────────────────────────────────────────────
        let t0 = Instant::now();
        let mut wire = match connect_wire(p.kind, p.addr, &p.tls, None).await {
            Ok(w) => w,
            Err(e) => {
                eprintln!("churn client {id}: connect failed: {e}");
                rep.errors += 1;
                tokio::time::sleep(cycle_end.saturating_duration_since(Instant::now())).await;
                continue;
            }
        };
        rep.connected = true;
        rep.connect_ms = t0.elapsed().as_millis();

        // -- auth, then the join below (its own frame, with retry) ────
        //    The AUTH states the wire version (DESIGN §5.5).
        let auth = session::auth_req(&creds);
        rep.bytes_out += frame_bytes(&wire, Dir::Out, auth.op, auth.payload.len());
        if wire.send(auth.op, &auth.payload).await.is_err() {
            continue;
        }

        // -- wait for JOIN_ROOM_RESULT (bounded, with bounded retry) ───
        let Some(entity) = churn_join(id, &mut wire, p.room, &mut rep).await else {
            // Never joined this cycle: nothing to park; just end it.
            drop(wire);
            tokio::time::sleep(cycle_end.saturating_duration_since(Instant::now())).await;
            continue;
        };
        rep.joined = true;
        if prev_entity == 0 {
            // First session: a plain join by definition.
        } else if entity == prev_entity {
            rep.resumed += 1; // SAME wire id: the park was consumed by a resume
        } else {
            rep.fresh_joins += 1; // different id: the hold had already ended (§5 fallback)
        }
        prev_entity = entity;

        // -- move until shortly before the cycle boundary, draining the
        //    inbound stream (the same interleaved shape as run_client) ─
        const DROP_MARGIN: Duration = Duration::from_millis(100);
        let phase_end = if final_session { p.deadline } else { cycle_end };
        let mut next_move = Instant::now();
        let mut seq: u64 = 1; // every session numbers inputs from 1 (§14.2 reset)
        loop {
            let now = Instant::now();
            if now + DROP_MARGIN >= phase_end {
                break;
            }
            if now >= next_move {
                next_move = now + p.move_ms;
                let (input_op, payload) = p.bot.churn_input(id, seq);
                seq += 1;
                rep.bytes_out += frame_bytes(&wire, Dir::Out, input_op, payload.len());
                if send_wire(&mut wire, input_op, payload).await.is_err() {
                    break; // peer/session gone early
                }
                rep.moves += 1;
            }
            let timeout = next_move
                .saturating_duration_since(Instant::now())
                .min(Duration::from_millis(250));
            match recv_wire(&mut wire, timeout).await {
                Got::Frame(op, payload) => {
                    rep.bytes_in += frame_bytes(&wire, Dir::In, op, payload.len());
                    if payload.is_empty() {
                        rep.errors += 1;
                    }
                    rep.snapshots += 1;
                }
                Got::Quiet => {}
                Got::Dead => break,
            }
        }

        // -- THE POINT: drop WITHOUT any LEAVE_ROOM_REQ. The reader pump
        //    sees EOF, the connection actor reports ConnClosed, the
        //    registry routes Detach, and the room PARKS the entity
        //    (inside the grace) — the next cycle's join resumes it. The
        //    FINAL session never drops: it keeps playing until the
        //    deadline (a resumed hero under sustained load).
        if final_session {
            break;
        }
        drop(wire);
        drops += 1;

        // Sleep out the rest of the cycle (the "player is away" window).
        tokio::time::sleep(cycle_end.saturating_duration_since(Instant::now())).await;
    }
    rep
}

#[cfg(test)]
mod tests;
