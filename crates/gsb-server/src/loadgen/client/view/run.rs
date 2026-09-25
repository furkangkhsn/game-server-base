//! One simulated client's whole life: connect, auth, join, move on
//! a timer, and report what it saw.

use super::*;
use gsb_kit::client::PrivateEvent;
use gsb_protocol::base::{
    Auth, Error, ErrorCode, JoinRoom, JoinRoomResult, LeaveRoom, LeaveRoomResult,
};
use gsb_protocol::op;
use prost::Message;
use std::time::{Duration, Instant};
use tokio::io::AsyncWriteExt;

pub(crate) async fn run_client(id: u64, p: ClientParams) -> ClientReport {
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

    // The game's bot for this client: its world view (the delta
    // protocol's client half — the kit's reference client, see
    // `ClientView`; fulls replace, deltas apply on top, a delta without a
    // baseline drops until the next full) and its input schedule.
    let mut bot = p.bot.client(id);
    let (snapshot_op, private_op) = (p.bot.snapshot_op(), p.bot.private_op());
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
    let mut wire = match connect_wire(p.kind, p.addr, &p.tls).await {
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
    // orders them).
    let auth_payload = Auth {
        name: crate::bot::bot_name(id),
        ticket: vec![],
        // A reference client states its wire version (DESIGN §5.5).
        protocol_version: gsb_protocol::PROTOCOL_VERSION,
    }
    .encode_to_vec();
    let join_payload = JoinRoom { room_id: p.room }.encode_to_vec();
    match &mut wire {
        Wire::Tcp { w, .. } => {
            let mut out = frame(op::base::AUTH_REQ, &auth_payload);
            out.extend(frame(op::base::JOIN_ROOM_REQ, &join_payload));
            rep.bytes_out += out.len() as u64;
            if w.write_all(&out).await.is_err() || w.flush().await.is_err() {
                return rep;
            }
        }
        Wire::Udp(c) => {
            rep.bytes_out += wire_in_bytes(op::base::AUTH_REQ, auth_payload.len());
            if c.send_frame(op::base::AUTH_REQ, auth_payload)
                .await
                .is_err()
            {
                return rep;
            }
            rep.bytes_out += wire_in_bytes(op::base::JOIN_ROOM_REQ, join_payload.len());
            if c.send_frame(op::base::JOIN_ROOM_REQ, join_payload)
                .await
                .is_err()
            {
                return rep;
            }
        }
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
    loop {
        let now = Instant::now();
        if now >= p.deadline {
            break;
        }
        if now.duration_since(last_move) >= p.move_ms {
            last_move = now;
            // The bot decides what this interval sends (possibly nothing:
            // a settled still client, a bot that has not seen its own
            // entity yet); a sent input takes the next number.
            if let Some((input_op, payload)) = bot.next_input(now.duration_since(t_start), next_seq)
            {
                next_seq += 1;
                sent_at.push(now);
                rep.moves += 1;
                match &mut wire {
                    Wire::Tcp { w, .. } => {
                        let f = frame(input_op, &payload);
                        rep.bytes_out += f.len() as u64;
                        if w.write_all(&f).await.is_err() || w.flush().await.is_err() {
                            break; // peer gone
                        }
                    }
                    Wire::Udp(c) => {
                        rep.bytes_out += wire_in_bytes(input_op, payload.len());
                        if c.send_frame(input_op, payload).await.is_err() {
                            break; // session gone (the writer gave up)
                        }
                    }
                }
            }
        }
        let timeout = p
            .deadline
            .saturating_duration_since(Instant::now())
            .min(Duration::from_millis(250));
        // TCP: None from read_frame = EOF (the loop breaks below); rUDP:
        // None = "quiet window" (no EOF exists — the deadline ends the
        // run instead).
        let got = match &mut wire {
            Wire::Tcp { r, .. } => tokio::time::timeout(timeout, read_frame(r.as_mut()))
                .await
                .ok()
                .flatten(),
            Wire::Udp(c) => c
                .recv_frame(timeout)
                .await
                .ok()
                .flatten()
                .map(|f| (f.op, f.payload.to_vec())),
        };
        let Some((op, payload)) = got else {
            continue; // timeout: loop
        };
        rep.bytes_in += match &wire {
            Wire::Tcp { .. } => (4 + 2 + payload.len()) as u64,
            Wire::Udp(_) => wire_in_bytes(op, payload.len()),
        };
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
                    Err(_) => rep.errors += 1,
                }
            }
            o if o == private_op => match bot.apply_private(&payload) {
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
                // No payload arm: this client sends no RPCs, so an empty
                // private frame is unexpected.
                Ok(PrivateEvent::Empty) => rep.errors += 1,
                // Undecodable, or a private DELTA — a protocol error (a
                // wrong-mode client must not silently misapply it).
                Err(_) => rep.errors += 1,
            },
            op::base::ERROR => {
                let e: Error = Error::decode(&payload[..]).unwrap_or_else(|_| Error::default());
                // Classified through the GENERATED enum (prost's
                // `code()` accessor), so this consumer and the server
                // cannot drift apart by hand-copied numbers.
                match e.code() {
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
                    // base.proto's forward-compatibility rule: an unknown
                    // or unspecified code is a plain error, never guessed
                    // onto a known decision.
                    _ => rep.errors += 1,
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
        // The input flood: write the bot's flood input (the demo's
        // MOVE_TO) as fast as the socket accepts,
        // until the deadline. The server-side chain (reader pump → conn
        // inbox → conn actor → action channel → room pull budget) bounds
        // what actually reaches the tick; the excess is dropped on the
        // flooder's OWN full action channel (attributed to it). The flood
        // stays UNNUMBERED (seq 0, legacy): it probes the drop-attribution
        // guardrails, not the sequence rule (a numbered flood would only
        // spin the high-water mark).
        let (flood_op, payload) = p.bot.flood_input();
        match &mut wire {
            Wire::Tcp { w, .. } => {
                let f = frame(flood_op, &payload);
                while Instant::now() < p.deadline {
                    if w.write_all(&f).await.is_err() {
                        break; // peer gone
                    }
                    rep.moves += 1;
                    rep.bytes_out += f.len() as u64;
                }
            }
            Wire::Udp(c) => {
                // rUDP: the client is ONE task (read and write share the
                // socket), so the flood interleaves NON-BLOCKING
                // read-drains; the flood frames travel the lossy game
                // band, so retransmit state never gets in the way.
                while Instant::now() < p.deadline {
                    if c.send_frame(flood_op, payload.clone()).await.is_err() {
                        break;
                    }
                    rep.moves += 1;
                    rep.bytes_out += wire_in_bytes(flood_op, payload.len());
                    while c.recv_frame(Duration::ZERO).await.ok().flatten().is_some() {}
                }
            }
        }
    }

    // Graceful leave (counted by the server's join/leave metrics) and
    // wait for the ack: without it, the socket close — and any server
    // shutdown that follows — can race ahead of the leave, and the
    // server never counts it. (rUDP: the leave is a control-band frame,
    // so it is retransmitted until the server ACKs it; there is no EOF
    // to race — the 500 ms window ends the wait.)
    let leave_payload = LeaveRoom {}.encode_to_vec();
    let leave_sent = match &mut wire {
        Wire::Tcp { w, .. } => {
            let f = frame(op::base::LEAVE_ROOM_REQ, &leave_payload);
            rep.bytes_out += f.len() as u64;
            w.write_all(&f).await.is_ok() && w.flush().await.is_ok()
        }
        Wire::Udp(c) => {
            rep.bytes_out += wire_in_bytes(op::base::LEAVE_ROOM_REQ, leave_payload.len());
            c.send_frame(op::base::LEAVE_ROOM_REQ, leave_payload)
                .await
                .is_ok()
        }
    };
    if leave_sent {
        let leave_deadline = Instant::now() + Duration::from_millis(500);
        while Instant::now() < leave_deadline {
            let timeout = leave_deadline.saturating_duration_since(Instant::now());
            let got = match &mut wire {
                Wire::Tcp { r, .. } => tokio::time::timeout(timeout, read_frame(r.as_mut()))
                    .await
                    .ok()
                    .flatten(),
                Wire::Udp(c) => c
                    .recv_frame(timeout)
                    .await
                    .ok()
                    .flatten()
                    .map(|f| (f.op, f.payload.to_vec())),
            };
            let Some((op, payload)) = got else {
                break;
            };
            rep.bytes_in += match &wire {
                Wire::Tcp { .. } => (4 + 2 + payload.len()) as u64,
                Wire::Udp(_) => wire_in_bytes(op, payload.len()),
            };
            if op == op::base::LEAVE_ROOM_RESULT {
                let _ = LeaveRoomResult::decode(&payload[..]);
                rep.left = true;
                break;
            }
        }
    }
    if let Some(c) = capture {
        c.finish().await;
    }
    // The client's rUDP transport statistics (all zero on TCP).
    if let Wire::Udp(c) = &wire {
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
