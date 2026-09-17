//! One simulated client's whole life: connect, auth, join, move on
//! a timer, and report what it saw.

use super::*;
use gsb_protocol::base::{
    Auth, Error, ErrorCode, JoinRoom, JoinRoomResult, LeaveRoom, LeaveRoomResult,
};
use gsb_protocol::op;
use prost::Message;
use std::collections::HashMap;
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

    // The client-side world view (the delta protocol's client half — see
    // `ClientView`; fulls replace, deltas apply on top, gaps drop until
    // the next full).
    let mut view = ClientView {
        entities: HashMap::new(),
        last_seq: None,
        cell_size: p.cell_size,
    };

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
        name: format!("lg-{id}"),
        ticket: vec![],
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
    // The still profile's deterministic per-id split (the same id-based
    // determinism as the stagger, 1% granularity — the id's hundredths
    // digit decides, so any client count gets the ratio): the first
    // `still_frac` fraction of the ids is "still" — it issues ONE MOVE_TO
    // (settling at a ring target) and then sends nothing more; the moving
    // minority chases the ring target as in the historical profile.
    let is_still = p.profile == Profile::Still && (id % 100) as f64 / 100.0 < p.still_frac;
    let mut settled = false;
    loop {
        let now = Instant::now();
        if now >= p.deadline {
            break;
        }
        if now.duration_since(last_move) >= p.move_ms {
            last_move = now;
            // A still client settles once: the first interval sends its
            // only command, the rest of the run it is silent (that IS the
            // profile — the majority of the world stands still).
            if !(is_still && settled) {
                settled = true;
                let (tx, ty) = match p.profile {
                    // The historical profile (UNCHANGED — all previous
                    // measurements stay comparable): a circle of radius 40
                    // around the map center at 4 rad/s, phase-shifted per
                    // client (id-based offset so N entities do not move in
                    // lockstep). The targets outrun the entities, so the
                    // entities crowd the central band — the *clustered*
                    // layout.
                    Profile::Ring => {
                        let angle =
                            (now.duration_since(t_start).as_secs_f64() + id as f64 * 0.618) * 4.0;
                        (angle.cos() * 40.0, angle.sin() * 40.0)
                    }
                    // The *spread* profile: each client wanders a small
                    // circle (radius 20, 0.4 rad/s — the target stays
                    // reachable at 10 u/s, so the entity tracks it closely)
                    // around its deterministic home, uniform over the
                    // ±spawn_half map. The layout stays ~uniform over the
                    // whole run (the server spawns with the same
                    // distribution), so there is no migration artifact and
                    // the run is statistically steady from tick 1. This is
                    // the sparse, wide-map layout a real MOBA arena
                    // resembles — where team fog actually hides most
                    // enemies (the clustered profile hides none).
                    Profile::Spread => {
                        let (hx, hy) = spawn_home(id, p.spawn_half);
                        let w =
                            (now.duration_since(t_start).as_secs_f64() + id as f64 * 0.618) * 0.4;
                        (hx + w.cos() * 20.0, hy + w.sin() * 20.0)
                    }
                    // The still profile's moving minority chases the ring
                    // target (the historical profile's shape).
                    Profile::Still => {
                        let angle =
                            (now.duration_since(t_start).as_secs_f64() + id as f64 * 0.618) * 4.0;
                        (angle.cos() * 40.0, angle.sin() * 40.0)
                    }
                };
                let seq = next_seq;
                next_seq += 1;
                sent_at.push(now);
                let msg = gsb_game::game::MoveTo {
                    x: tx as i32,
                    y: ty as i32,
                    seq,
                };
                let move_payload = msg.encode_to_vec();
                rep.moves += 1;
                match &mut wire {
                    Wire::Tcp { w, .. } => {
                        let f = frame(gsb_game::op::MOVE_TO, &move_payload);
                        rep.bytes_out += f.len() as u64;
                        if w.write_all(&f).await.is_err() || w.flush().await.is_err() {
                            break; // peer gone
                        }
                    }
                    Wire::Udp(c) => {
                        rep.bytes_out += wire_in_bytes(gsb_game::op::MOVE_TO, move_payload.len());
                        if c.send_frame(gsb_game::op::MOVE_TO, move_payload)
                            .await
                            .is_err()
                        {
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
        match op {
            op::base::JOIN_ROOM_RESULT => {
                let m: JoinRoomResult = match JoinRoomResult::decode(&payload[..]) {
                    Ok(m) => m,
                    Err(_) => continue,
                };
                rep.joined = true;
                rep.entity = m.entity;
                if p.flood {
                    // Flood mode: leave the paced loop; the tight write
                    // loop below runs until the deadline.
                    flooded = true;
                    break;
                }
            }
            gsb_game::op::WORLD_SNAPSHOT => {
                match gsb_game::game::WorldSnapshot::decode(&payload[..]) {
                    Ok(m) => {
                        rep.snapshots += 1;
                        let at = Instant::now();
                        if rep.seq_first.is_none() {
                            rep.seq_first = Some((m.sequence, at));
                        }
                        rep.seq_last = Some((m.sequence, at));
                        // The client half of the delta protocol (see
                        // `ClientView`): apply it, whatever the strategy's
                        // mode (a full-snapshot room's frames are all fulls).
                        match view.apply(&m) {
                            Apply::Full => rep.fulls += 1,
                            Apply::Delta => rep.deltas += 1,
                            Apply::NoBaseline => rep.gap_drops += 1,
                            Apply::Stale => {}
                        }
                    }
                    Err(_) => rep.errors += 1,
                }
            }
            gsb_game::op::PRIVATE => {
                let pr = match gsb_game::game::Private::decode(&payload[..]) {
                    Ok(pr) => pr,
                    Err(_) => {
                        rep.errors += 1;
                        continue;
                    }
                };
                match pr.payload {
                    Some(gsb_game::game::private::Payload::Ack(ack)) => {
                        // Section A: the server's per-connection input
                        // high-water mark. `now` is this loop iteration's
                        // instant — the ack's lag is measured against the
                        // send instant of the acked seq (index seq-1).
                        rep.acks += 1;
                        rep.ack_processed_max = rep.ack_processed_max.max(ack.processed_up_to);
                        if ack.processed_up_to > 0 {
                            let i = ack.processed_up_to as usize - 1;
                            if i < sent_at.len() {
                                let lag = Instant::now().duration_since(sent_at[i]);
                                rep.ack_lag_max_ms = rep.ack_lag_max_ms.max(lag.as_millis());
                            }
                        }
                    }
                    Some(gsb_game::game::private::Payload::Snapshot(sn)) => {
                        // A one-shot FULL view (a fresh group member — late
                        // join or a group crossing). It MUST be a full: a
                        // delta here would be a protocol error, and a
                        // wrong-mode client must not silently misapply it.
                        if sn.delta {
                            rep.errors += 1;
                        } else {
                            rep.private_fulls += 1;
                            rep.fulls += 1;
                            view.apply_private_full(&sn);
                        }
                    }
                    None => rep.errors += 1,
                }
            }
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

    // The final client view size (the delta protocol's end state).
    rep.view_size = view.entities.len() as u64;

    if flooded {
        // The input flood: write MOVE_TO as fast as the socket accepts,
        // until the deadline. The server-side chain (reader pump → conn
        // inbox → conn actor → action channel → room pull budget) bounds
        // what actually reaches the tick; the excess is dropped on the
        // flooder's OWN full action channel (attributed to it). The flood
        // stays UNNUMBERED (seq 0, legacy): it probes the drop-attribution
        // guardrails, not the sequence rule (a numbered flood would only
        // spin the high-water mark).
        let msg = gsb_game::game::MoveTo { x: 0, y: 0, seq: 0 };
        match &mut wire {
            Wire::Tcp { w, .. } => {
                let f = frame(gsb_game::op::MOVE_TO, &msg.encode_to_vec());
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
                let payload = msg.encode_to_vec();
                while Instant::now() < p.deadline {
                    if c.send_frame(gsb_game::op::MOVE_TO, payload.clone())
                        .await
                        .is_err()
                    {
                        break;
                    }
                    rep.moves += 1;
                    rep.bytes_out += wire_in_bytes(gsb_game::op::MOVE_TO, payload.len());
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
    // The client's rUDP transport statistics (all zero on TCP).
    if let Wire::Udp(c) = &wire {
        rep.retrans_out = c.stats.retrans_out;
        rep.dup_in = c.stats.dup_in;
        rep.oob_dropped = c.stats.oob_dropped;
        rep.gave_up = c.stats.gave_up;
    }
    rep
}
