//! One simulated client's whole life: connect, auth, join, move on
//! a timer, and report what it saw.

use super::*;
use gsb_client::session::{self, Credentials};
use gsb_client::{Conn, Recv, ServerError};
use gsb_kit::client::PrivateEvent;
use gsb_protocol::base::{ErrorCode, JoinRoomResult};
use gsb_protocol::op;
use prost::Message;
use std::time::{Duration, Instant};

pub(crate) async fn run_client(id: u64, p: ClientParams) -> ClientReport {
    let mut rep = ClientReport::new(id);

    // The game's bot for this client: its world view (the delta
    // protocol's client half — the kit's reference client, see
    // `ClientView`; fulls replace, deltas apply on top, a delta without a
    // baseline drops until the next full) and its input schedule.
    let mut bot = p.bot.client(id);
    let (snapshot_op, private_op) = (p.bot.snapshot_op(), p.bot.private_op());
    // `--rpc-rate`: the requests beside the inputs, and their ledger.
    let mut rpc = p.rpc.map(|plan| RpcClient::new(plan, id));
    // `--capture`: this client's game-band frames, recorded as received
    // (a measurement aid — nothing it records changes what is applied).
    let mut capture = p
        .capture
        .clone()
        .map(|(path, game)| crate::capture::Capture::new(path, game, id));

    // Optional connect stagger (see `Args::stagger_ms`).
    if p.stagger_ms > 0.0 {
        tokio::time::sleep(Duration::from_secs_f64(id as f64 * p.stagger_ms / 1000.0)).await;
    }
    // The wire to the server: the ONLY place the two transports diverge
    // inside the client loop (everything above and below it is
    // transport-agnostic). On rUDP `connect` is the cookie handshake, so
    // `connect_ms` measures the handshake latency.
    let t0 = Instant::now();
    let mut wire = match connect_wire(p.kind, p.addr, &p.tls, p.stall.map(|_| STALL_RCVBUF)).await {
        Ok(w) => w,
        Err(e) => {
            eprintln!("client {id}: connect failed: {e}");
            return rep;
        }
    };
    rep.connect_ms = t0.elapsed().as_millis();
    rep.connected = true;

    // AUTH + JOIN (one coalesced write on TCP — the connection actor
    // drains in order; two frames on rUDP — its reliable control band
    // orders them). The AUTH states the wire version (DESIGN §5.5).
    let (auth, join) = (
        session::auth_req(&Credentials::named(crate::bot::bot_name(id))),
        session::join_req(p.room),
    );
    rep.bytes_out += frame_bytes(&wire, Dir::Out, auth.op, auth.payload.len());
    rep.bytes_out += frame_bytes(&wire, Dir::Out, join.op, join.payload.len());
    if wire.send_batch(&[auth, join]).await.is_err() {
        return rep;
    }

    let t_start = Instant::now();
    let mut last_move = t_start;
    let mut flooded = false;
    // Section A: inputs are NUMBERED (monotonic from 1 per session); the
    // server acks its per-connection high-water mark in the Private frame.
    // `sent_at` keeps the send instant of seq N at index N-1 (for the
    // ack-lag measurement) — one entry per numbered input, a few dozen
    // per client per run.
    let mut next_seq: u64 = 1;
    let mut sent_at: Vec<Instant> = Vec::new();
    // Past the deadline a client still owed its JOIN's answer keeps
    // reading for it (bounded, in its own waiting time): a starved
    // client used to stop with the answer in its socket — `joined=0` on
    // a run whose server had joined all twelve (BACKLOG F51).
    let mut settle = Wait::new(PROTOCOL_WAIT);
    loop {
        let now = Instant::now();
        let playing = now < p.deadline;
        if !playing && join_answered(&rep) {
            break;
        }
        // Inputs wait for the JOIN's answer (B88): before it the session
        // is in no room, and a game frame there is answered `NotInRoom`
        // (on rUDP the lossy game band overtakes the reliable JOIN).
        if playing && rep.joined && now.duration_since(last_move) >= p.move_ms {
            last_move = now;
            // The bot decides what this interval sends (possibly nothing:
            // a settled still client, a bot that has not seen its own
            // entity yet); a sent input takes the next number.
            if let Some((input_op, payload)) = bot.next_input(now.duration_since(t_start), next_seq)
            {
                next_seq += 1;
                sent_at.push(now);
                rep.moves += 1;
                rep.bytes_out += frame_bytes(&wire, Dir::Out, input_op, payload.len());
                if wire.send(input_op, &payload).await.is_err() {
                    break; // peer gone (rUDP: the writer gave up)
                }
            }
        }
        if let Some(burst) = rpc.as_mut().filter(|_| playing).and_then(|r| r.due(now)) {
            for f in &burst {
                rep.bytes_out += frame_bytes(&wire, Dir::Out, f.op, f.payload.len());
            }
            if wire.send_batch(&burst).await.is_err() {
                break; // peer gone
            }
        }
        // The next burst bounds every wait below, so a quiet socket (or
        // a slow reader's pause) does not hold the requests back.
        let rpc_wait = rpc
            .as_ref()
            .and_then(|r| r.until_due(now))
            .unwrap_or(Duration::MAX);
        if let Some(left) = p
            .stall
            .filter(|_| playing)
            .and_then(|s| s.pause_left(id, now.duration_since(t_start)))
        {
            // The slow reader (`--stall-ms`): away from the socket (but
            // still sending — the next burst ends the nap).
            let nap = left.min(rpc_wait);
            tokio::time::sleep(nap.min(p.deadline.saturating_duration_since(now))).await;
            continue;
        }
        let timeout = if playing {
            p.deadline
                .saturating_duration_since(Instant::now())
                .min(Duration::from_millis(250))
                .min(rpc_wait)
        } else {
            match settle.slice() {
                Some(slice) => slice,
                None => break, // the JOIN's answer never came
            }
        };
        // A quiet window (and an rUDP socket error, which is all rUDP
        // can report) loops; a stream's EOF or a frame its reader
        // refuses ends the session — the leave below then finds the
        // wire dead.
        let at = Instant::now();
        let got = recv_wire(&mut wire, timeout).await;
        if !playing {
            settle.charge(timeout, at);
        }
        let (op, payload) = match got {
            Got::Frame(op, payload) => (op, payload),
            Got::Quiet => continue,
            Got::Dead => break,
        };
        rep.bytes_in += frame_bytes(&wire, Dir::In, op, payload.len());
        if let Some(c) = &mut capture {
            if op == snapshot_op {
                c.frame(crate::capture::Kind::Snapshot, &payload);
            } else if op == private_op {
                c.frame(crate::capture::Kind::Private, &payload);
            } else if op == op::base::JOIN_ROOM_RESULT {
                c.frame(crate::capture::Kind::Joined, &payload);
            }
        }
        match op {
            op::base::JOIN_ROOM_RESULT => {
                let m: JoinRoomResult = match JoinRoomResult::decode(&payload[..]) {
                    Ok(m) => m,
                    Err(_) => continue,
                };
                rep.joined = true;
                rep.entity = m.entity;
                bot.joined(m.entity);
                if let Some(r) = &mut rpc {
                    r.joined(Instant::now());
                }
                if p.flood {
                    // Flood mode: leave the paced loop; the tight write
                    // loop below runs until the deadline.
                    flooded = true;
                    break;
                }
            }
            o if o == snapshot_op => {
                // The client half of the delta protocol (see
                // `ClientView`): apply it, whatever the strategy's mode (a
                // full-snapshot room's frames are all fulls). The view
                // counts fulls / deltas / no-baseline drops itself.
                match bot.apply_snapshot(&payload) {
                    Ok(s) => {
                        rep.snapshots += 1;
                        let at = Instant::now();
                        if rep.seq_first.is_none() {
                            rep.seq_first = Some((s.sequence, at));
                        }
                        rep.seq_last = Some((s.sequence, at));
                    }
                    Err(_) => rep.errors.bad_snapshot += 1,
                }
            }
            o if o == private_op => {
                // The RPC answers the frame carries (the view ignores
                // them — `Private.responses` is not part of it).
                let answers = rpc
                    .as_mut()
                    .map_or(0, |r| r.on_private(&payload, Instant::now()));
                match bot.apply_private(&payload) {
                    Ok(PrivateEvent::Ack(up_to)) => {
                        // Section A: the server's per-connection input
                        // high-water mark. `now` is this loop iteration's
                        // instant — the ack's lag is measured against the
                        // send instant of the acked seq (index seq-1).
                        rep.acks += 1;
                        rep.ack_processed_max = rep.ack_processed_max.max(up_to);
                        if up_to > 0 {
                            let i = up_to as usize - 1;
                            if i < sent_at.len() {
                                let lag = Instant::now().duration_since(sent_at[i]);
                                rep.ack_lag_max_ms = rep.ack_lag_max_ms.max(lag.as_millis());
                            }
                        }
                    }
                    // A one-shot FULL view (a fresh group member — late join
                    // or a group crossing), applied by the view (counted in
                    // its fulls and private fulls).
                    Ok(PrivateEvent::Full { .. }) => {}
                    // The game's session payload alone (the arena's welcome),
                    // handed to the bot's decoder by the view.
                    Ok(PrivateEvent::Session) => {}
                    // No payload arm: a frame of RPC answers alone — anything
                    // else empty is unexpected.
                    Ok(PrivateEvent::Empty) if answers > 0 => {}
                    Ok(PrivateEvent::Empty) => rep.errors.bad_private += 1,
                    // Undecodable, or a private DELTA — a protocol error (a
                    // wrong-mode client must not silently misapply it).
                    Err(_) => rep.errors.bad_private += 1,
                }
            }
            op::base::ERROR => {
                // Classified through the GENERATED enum (an unknown code
                // reads as `Unspecified`), so this consumer and the
                // server cannot drift apart by hand-copied numbers.
                let e = ServerError::decode_lossy(&payload);
                match e.code {
                    // The guardrails, observed from the client side:
                    // RoomFull = gentle reject, the connection stays;
                    // ServerClosed = the server closed us — either the
                    // connection-capacity cap or the protocol-violation
                    // budget (same class; the message separates them).
                    ErrorCode::RoomFull => rep.join_rejected += 1,
                    ErrorCode::ServerClosed if e.message.contains("violation") => {
                        rep.budget_rejected += 1
                    }
                    ErrorCode::ServerClosed => rep.cap_rejected += 1,
                    // A frame the server read outside any room (B88).
                    ErrorCode::NotInRoom => rep.errors.not_in_room += 1,
                    // base.proto's forward-compatibility rule: an unknown
                    // or unspecified code is a plain error, never guessed
                    // onto a known decision.
                    _ => rep.errors.other_code += 1,
                }
            }
            _ => {}
        }
    }

    // The view's counters and final size (the delta protocol's end
    // state).
    let c = bot.counters();
    rep.fulls = c.fulls;
    rep.private_fulls = c.private_fulls;
    rep.deltas = c.deltas;
    rep.gap_drops = c.gap_drops;
    rep.view_size = bot.view_len() as u64;

    if flooded {
        end::flood(&mut wire, &p, &mut rep).await;
    }
    if rep.joined {
        end::leave(&mut wire, &mut rep, rpc.as_mut(), private_op).await;
    }
    if let Some(r) = rpc {
        rep.rpc = r.finish(Instant::now());
    }
    if let Some(c) = capture {
        c.finish().await;
    }
    // The client's rUDP transport statistics (all zero on TCP).
    if let Some(c) = wire.udp_client() {
        rep.retrans_out = c.stats.retrans_out;
        rep.dup_in = c.stats.dup_in;
        rep.oob_dropped = c.stats.oob_dropped;
        rep.gave_up = c.stats.gave_up;
        rep.frag_reassembled = c.stats.frag_reassembled;
        rep.frag_dropped = c.stats.frag_dropped_incomplete;
        rep.hs_retries = c.stats.challenge_retries + c.stats.proof_retries;
    }
    rep
}

/// Whether the JOIN has been answered: seated, or turned away by a
/// guardrail.
fn join_answered(rep: &ClientReport) -> bool {
    rep.joined || rep.join_rejected + rep.cap_rejected + rep.budget_rejected > 0
}

mod end;

#[cfg(test)]
mod tests;
