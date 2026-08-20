//! End-to-end test: an in-process gsb server on an ephemeral port, a real
//! TCP client that authenticates, joins room 1, issues a move, and asserts
//! that it observes its entity's position change in the world snapshots.

use std::time::{Duration, Instant};

use gsb_protocol::base::{Auth, AuthResult, Error, Heartbeat, HeartbeatAck, JoinRoom, JoinRoomResult};
use prost::Message;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

fn frame(op: u16, msg: &impl Message) -> Vec<u8> {
    let payload = msg.encode_to_vec();
    let body = 2 + payload.len();
    let mut out = Vec::with_capacity(4 + body);
    out.extend_from_slice(&(body as u32).to_le_bytes());
    out.extend_from_slice(&op.to_le_bytes());
    out.extend_from_slice(&payload);
    out
}

async fn read_frame<R: AsyncRead + Unpin>(stream: &mut R) -> Option<(u16, Vec<u8>)> {
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf).await.ok()?;
    let len = u32::from_le_bytes(len_buf) as usize;
    if len == 0 || len > 4 * 1024 * 1024 {
        return None;
    }
    let mut body = vec![0u8; len];
    stream.read_exact(&mut body).await.ok()?;
    if body.len() < 2 {
        return None;
    }
    let op = u16::from_le_bytes([body[0], body[1]]);
    Some((op, body[2..].to_vec()))
}

#[tokio::test]
async fn client_joins_and_receives_snapshots() {
    // In-process server on an ephemeral port.
    let cfg = gsb_server::Config {
        bind: "127.0.0.1:0".into(),
        room_count: 1,
        ..Default::default()
    };
    let handle = gsb_server::start_server(cfg).await.expect("server starts");
    let addr = handle.addr;

    let mut stream = TcpStream::connect(addr).await.expect("client connects");
    stream
        .write_all(&frame(
            gsb_protocol::op::base::AUTH_REQ,
            &Auth { name: "e2e".into() },
        ))
        .await
        .unwrap();
    stream
        .write_all(&frame(
            gsb_protocol::op::base::JOIN_ROOM_REQ,
            &JoinRoom { room_id: 1 },
        ))
        .await
        .unwrap();
    stream.flush().await.unwrap();

    // The room ships self-contained world snapshots (one per change, plus
    // a low-rate keep-alive). Success = we first observe our entity, then
    // observe its position change (the move propagated: action → ingest →
    // movement system → snapshot → writer pump).
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut my_entity: u64 = 0;
    let mut move_sent = false;
    let mut first_pos: Option<(i32, i32)> = None;

    loop {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .unwrap_or_else(|| panic!("timed out (move_sent={move_sent})"));
        let (op, payload) = match tokio::time::timeout(remaining, read_frame(&mut stream)).await {
            Ok(Some(f)) => f,
            Ok(None) => panic!("server closed the connection"),
            Err(_) => panic!("timed out waiting for frames"),
        };

        match op {
            gsb_protocol::op::base::AUTH_RESULT => {
                let m: AuthResult = AuthResult::decode(&payload[..]).unwrap();
                assert!(m.ok, "auth must succeed");
            }
            gsb_protocol::op::base::JOIN_ROOM_RESULT => {
                let m: JoinRoomResult = JoinRoomResult::decode(&payload[..]).unwrap();
                my_entity = m.entity;
                assert!(my_entity != 0, "entity id must be non-zero");
                // Force movement so the room re-emits a snapshot.
                stream
                    .write_all(&frame(
                        gsb_game::op::MOVE_TO,
                        &gsb_game::game::MoveTo { x: 10, y: 10 },
                    ))
                    .await
                    .unwrap();
                move_sent = true;
            }
            gsb_protocol::op::base::ERROR => {
                let m: Error = Error::decode(&payload[..]).unwrap();
                panic!("server error: code={} message={}", m.code, m.message);
            }
            gsb_game::op::WORLD_SNAPSHOT => {
                let m: gsb_game::game::WorldSnapshot =
                    gsb_game::game::WorldSnapshot::decode(&payload[..]).unwrap();
                assert!(
                    m.sequence > 0,
                    "snapshot sequence must be monotonic (> 0)"
                );
                let Some(rec) = m.entities.iter().find(|e| e.entity == my_entity) else {
                    continue; // snapshot that arrived before the join result
                };
                let pos = (rec.x, rec.y);
                match first_pos {
                    None => first_pos = Some(pos),
                    Some(first) => {
                        if pos != first {
                            break; // moved: the full path is proven
                        }
                        // Still at the spawn position: keep reading.
                    }
                }
            }
            other => {
                panic!("unexpected op {other} in e2e handshake");
            }
        }
    }

    assert!(move_sent, "must have been able to send MOVE_TO");
    handle.stop().await;
}

// ── hardening-round e2e: session lifecycle, capacity, fairness ─────────

/// A server config with the guardrail overrides this test needs.
fn guardrail_cfg(
    idle_timeout_secs: Option<f64>,
    max_players: Option<u32>,
    max_connections: Option<u64>,
) -> gsb_server::Config {
    let mut cfg = gsb_server::Config {
        bind: "127.0.0.1:0".into(),
        room_count: 1,
        ..Default::default()
    };
    if let Some(s) = idle_timeout_secs {
        cfg.idle_timeout_secs = s;
    }
    if let Some(n) = max_players {
        cfg.max_players = Some(n);
    }
    if let Some(n) = max_connections {
        cfg.max_connections = Some(n);
    }
    cfg
}

/// Session-lifecycle guardrail (item 1): a client that authenticates and
/// then says nothing is closed by the server on its own initiative — a
/// gentle `ERROR` (code 9) first, then EOF. Without the reader-pump idle
/// window this connection (and its tasks/channels/registry entry) would
/// sit until process death: the half-open TCP case.
#[tokio::test]
async fn idle_connection_is_closed_by_the_server() {
    let handle = gsb_server::start_server(guardrail_cfg(Some(1.0), None, None))
        .await
        .expect("server starts");
    let mut stream = TcpStream::connect(handle.addr)
        .await
        .expect("client connects");
    stream
        .write_all(&frame(
            gsb_protocol::op::base::AUTH_REQ,
            &Auth { name: "idle".into() },
        ))
        .await
        .unwrap();
    stream.flush().await.unwrap();

    // Stay silent. The deadline arms at the last inbound frame (the auth)
    // and fires after the 1 s window. Tolerate outbound frames (the room
    // keep-alive) — we are looking for the server's own close: ERROR 9,
    // then EOF.
    let deadline = Instant::now() + Duration::from_secs(8);
    let mut got_server_closed_error = false;
    let mut closed = false;
    while !closed && Instant::now() < deadline {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .unwrap_or_else(|| panic!("timed out (got_server_closed_error={got_server_closed_error})"));
        let (op, payload) = match tokio::time::timeout(remaining, read_frame(&mut stream)).await {
            Ok(Some(f)) => f,
            Ok(None) => {
                closed = true;
                break;
            }
            Err(_) => panic!("timed out waiting for the server close"),
        };
        if op == gsb_protocol::op::base::ERROR {
            let m: Error = Error::decode(&payload[..]).unwrap();
            assert_eq!(m.code, 9, "server-initiated close is code 9: {m:?}");
            got_server_closed_error = true;
        }
    }
    assert!(closed, "the idle connection must reach EOF");
    assert!(
        got_server_closed_error,
        "the close must be a gentle ERROR 9 BEFORE EOF, not a bare drop"
    );
    handle.stop().await;
}

/// Session-lifecycle guardrail (item 1), positive side: a client that
/// keeps any inbound traffic flowing (here: heartbeats every 250 ms
/// against a 1 s window) is never touched — its window resets on every
/// frame, and it keeps getting its heartbeat acks for the whole run.
#[tokio::test]
async fn active_heartbeat_survives_the_idle_window() {
    let handle = gsb_server::start_server(guardrail_cfg(Some(1.0), None, None))
        .await
        .expect("server starts");
    let mut stream = TcpStream::connect(handle.addr)
        .await
        .expect("client connects");
    stream
        .write_all(&frame(
            gsb_protocol::op::base::AUTH_REQ,
            &Auth { name: "active".into() },
        ))
        .await
        .unwrap();
    stream.flush().await.unwrap();

    // 3.5 s of 250 ms heartbeats against a 1 s window: 5+ resets.
    let t0 = Instant::now();
    let mut acks = 0u64;
    let mut next_hb = t0;
    loop {
        let elapsed = t0.elapsed();
        if elapsed >= Duration::from_millis(3500) {
            break;
        }
        if Instant::now() >= next_hb {
            next_hb += Duration::from_millis(250);
            stream
                .write_all(&frame(
                    gsb_protocol::op::base::HEARTBEAT,
                    &Heartbeat { tick: acks },
                ))
                .await
                .expect("socket alive")
            ;
            stream.flush().await.expect("socket alive");
        }
        let (op, payload) = match tokio::time::timeout(
            Duration::from_millis(500),
            read_frame(&mut stream),
        )
        .await
        {
            Ok(Some(f)) => f,
            Ok(None) => panic!("server closed an active connection"),
            Err(_) => continue, // quiet window: no ack this round
        };
        if op == gsb_protocol::op::base::HEARTBEAT_ACK {
            let _m: HeartbeatAck = HeartbeatAck::decode(&payload[..]).unwrap();
            acks += 1;
        }
    }
    assert!(acks >= 3, "heartbeats were answered throughout: {acks} acks");
    handle.stop().await;
}

/// Capacity guardrail (item 2), gentle rejection: with `max_players = 1`,
/// the second joiner gets `ERROR` code 8 — and its connection STAYS ALIVE
/// (it can pick another room or retry; a silent close would masquerade as
/// a network failure and trigger reconnect storms against a busy server).
#[tokio::test]
async fn room_full_returns_gentle_error_code_8() {
    let handle = gsb_server::start_server(guardrail_cfg(None, Some(1), None))
        .await
        .expect("server starts");
    let addr = handle.addr;

    // Player A takes the only seat.
    let mut a = TcpStream::connect(addr).await.expect("A connects");
    let mut a_out = Vec::new();
    a_out.extend(frame(gsb_protocol::op::base::AUTH_REQ, &Auth { name: "a".into() }));
    a_out.extend(frame(gsb_protocol::op::base::JOIN_ROOM_REQ, &JoinRoom { room_id: 1 }));
    a.write_all(&a_out).await.unwrap();
    a.flush().await.unwrap();
    let mut a_joined = false;
    let deadline = Instant::now() + Duration::from_secs(5);
    while !a_joined {
        let (op, _payload) = match tokio::time::timeout(
            deadline.checked_duration_since(Instant::now()).unwrap_or_else(|| {
                panic!("A did not join within 5 s")
            }),
            read_frame(&mut a),
        )
        .await
        {
            Ok(Some(f)) => f,
            _ => panic!("A's connection ended before the join"),
        };
        a_joined = op == gsb_protocol::op::base::JOIN_ROOM_RESULT;
    }

    // Player B is rejected (code 8)…
    let mut b = TcpStream::connect(addr).await.expect("B connects");
    let mut b_out = Vec::new();
    b_out.extend(frame(gsb_protocol::op::base::AUTH_REQ, &Auth { name: "b".into() }));
    b_out.extend(frame(gsb_protocol::op::base::JOIN_ROOM_REQ, &JoinRoom { room_id: 1 }));
    b.write_all(&b_out).await.unwrap();
    b.flush().await.unwrap();
    let mut b_rejected = false;
    let deadline = Instant::now() + Duration::from_secs(5);
    while !b_rejected {
        let (op, payload) = match tokio::time::timeout(
            deadline.checked_duration_since(Instant::now()).unwrap_or_else(|| {
                panic!("B was not rejected within 5 s")
            }),
            read_frame(&mut b),
        )
        .await
        {
            Ok(Some(f)) => f,
            _ => panic!("B's connection ended before the rejection"),
        };
        if op == gsb_protocol::op::base::ERROR {
            let m: Error = Error::decode(&payload[..]).unwrap();
            assert_eq!(m.code, 8, "room-full rejection is code 8: {m:?}");
            b_rejected = true;
        }
    }

    // …and B stays alive: the rejection is gentle, not a drop.
    b.write_all(&frame(
        gsb_protocol::op::base::HEARTBEAT,
        &Heartbeat { tick: 7 },
    ))
    .await
    .expect("B's socket still alive");
    b.flush().await.unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut acked = false;
    while !acked {
        let (op, payload) = match tokio::time::timeout(
            deadline.checked_duration_since(Instant::now()).unwrap_or_else(|| {
                panic!("B did not get an ack after the gentle rejection")
            }),
            read_frame(&mut b),
        )
        .await
        {
            Ok(Some(f)) => f,
            _ => panic!("B's connection was dropped after all"),
        };
        if op == gsb_protocol::op::base::HEARTBEAT_ACK {
            let m: HeartbeatAck = HeartbeatAck::decode(&payload[..]).unwrap();
            assert_eq!(m.tick, 7, "the ack echoes the heartbeat tick");
            acked = true;
        }
    }
    handle.stop().await;
}

/// Capacity guardrail (item 2), server-wide cap: with
/// `max_connections = 1`, the second connection is rejected at birth —
/// `ERROR` code 9 followed by EOF (this one IS a close: the registry has
/// no seat for it, so the connection cannot exist). The first connection
/// is unaffected.
#[tokio::test]
async fn connection_capacity_rejects_with_code_9() {
    let handle = gsb_server::start_server(guardrail_cfg(None, None, Some(1)))
        .await
        .expect("server starts");
    let addr = handle.addr;

    // A takes the only seat (and auths, so it is fully live).
    let mut a = TcpStream::connect(addr).await.expect("A connects");
    a.write_all(&frame(
        gsb_protocol::op::base::AUTH_REQ,
        &Auth { name: "a".into() },
    ))
    .await
    .unwrap();
    a.flush().await.unwrap();

    // B is rejected at birth: ERROR 9, then EOF.
    let mut b = TcpStream::connect(addr).await.expect("B connects");
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut got9 = false;
    let mut closed = false;
    while !closed && Instant::now() < deadline {
        let (op, payload) = match tokio::time::timeout(
            deadline.checked_duration_since(Instant::now()).unwrap_or_else(|| {
                panic!("timed out (got9={got9})")
            }),
            read_frame(&mut b),
        )
        .await
        {
            Ok(Some(f)) => f,
            Ok(None) => {
                closed = true;
                break;
            }
            Err(_) => panic!("timed out"),
        };
        if op == gsb_protocol::op::base::ERROR {
            let m: Error = Error::decode(&payload[..]).unwrap();
            assert_eq!(m.code, 9, "capacity rejection is code 9: {m:?}");
            got9 = true;
        }
    }
    assert!(got9, "B must see the capacity ERROR before EOF");
    assert!(closed, "B must reach EOF (the registry has no seat for it)");

    // A is unaffected by B's rejection.
    a.write_all(&frame(
        gsb_protocol::op::base::HEARTBEAT,
        &Heartbeat { tick: 1 },
    ))
    .await
    .expect("A still alive");
    a.flush().await.unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut acked = false;
    while !acked {
        let (op, _payload) = match tokio::time::timeout(
            deadline.checked_duration_since(Instant::now()).unwrap_or_else(|| {
                panic!("A did not get an ack")
            }),
            read_frame(&mut a),
        )
        .await
        {
            Ok(Some(f)) => f,
            _ => panic!("A's connection ended"),
        };
        acked = op == gsb_protocol::op::base::HEARTBEAT_ACK;
    }
    handle.stop().await;
}

/// Fairness guardrail (item 3), attribution: a single client that floods
/// MOVE_TO in a tight loop overflows ITS OWN bounded action channel —
/// the room (bounded pull) never drops, and the drops are counted per
/// connection and surface in the net scope attributed to the flooder
/// (`actions_dropped_top`).
#[tokio::test]
async fn flooder_drops_are_attributed_to_the_flooder() {
    let cfg = gsb_server::Config {
        bind: "127.0.0.1:0".into(),
        room_count: 1,
        ..Default::default()
    };
    let (rep_tx, rep_rx) = tokio::sync::mpsc::unbounded_channel::<gsb_core::metrics::MetricReport>();
    let handle = gsb_server::start_server_metrics(cfg, rep_tx).await.expect("server starts");

    // The flooder: auth + join, then a DEDICATED tight-write task (no
    // read pacing — interleaving reads would slow the flood below the
    // room's 480/s pull budget and never fill the channel).
    let stream = TcpStream::connect(handle.addr).await.expect("client connects");
    let (mut r, mut w) = stream.into_split();
    let mut out = Vec::new();
    out.extend(frame(gsb_protocol::op::base::AUTH_REQ, &Auth { name: "flood".into() }));
    out.extend(frame(gsb_protocol::op::base::JOIN_ROOM_REQ, &JoinRoom { room_id: 1 }));
    w.write_all(&out).await.unwrap();
    w.flush().await.unwrap();

    // Wait for the join, then start the flood.
    let mut joined = false;
    let deadline = Instant::now() + Duration::from_secs(5);
    while !joined {
        let (op, _payload) = match tokio::time::timeout(
            deadline.checked_duration_since(Instant::now()).unwrap_or_else(|| {
                panic!("the flooder never joined")
            }),
            read_frame(&mut r),
        )
        .await
        {
            Ok(Some(f)) => f,
            Ok(None) => panic!("server closed the flooder before the join"),
            Err(_) => panic!("timed out"),
        };
        joined = op == gsb_protocol::op::base::JOIN_ROOM_RESULT;
    }

    let flood_start = Instant::now();
    let flood = tokio::spawn(async move {
        // As fast as the socket accepts: the action channel (cap 256)
        // fills almost instantly against the room's 16/tick pull budget.
        let fdeadline = flood_start + Duration::from_secs(3);
        let mf = frame(
            gsb_game::op::MOVE_TO,
            &gsb_game::game::MoveTo { x: 1, y: 1 },
        );
        while Instant::now() < fdeadline {
            if w.write_all(&mf).await.is_err() {
                break; // peer gone
            }
        }
    });

    // Drain whatever arrives while the flood runs (snapshots, keep-alives).
    while flood_start.elapsed() < Duration::from_secs(3) {
        let _ = tokio::time::timeout(Duration::from_millis(200), read_frame(&mut r)).await;
    }
    flood.await.expect("flood task exits");

    // Stop the server: the collector's final flush lands during shutdown,
    // and its sender drop then closes the channel — `recv` drains until
    // the last report is out.
    handle.stop().await;
    let mut rx = rep_rx;
    let mut reports = Vec::new();
    while let Some(r) = rx.recv().await {
        reports.push(r);
    }
    let last = reports.last().expect("at least one metric report");
    assert!(
        last.net.actions_dropped > 0,
        "the flood must have overflowed the flooder's action channel"
    );
    assert_eq!(
        last.actions_dropped_top.first(),
        Some(&(gsb_core::id::ConnectionId(1), last.net.actions_dropped)),
        "the sole connection (id 1) is the sole dropper: every drop is \
         attributed to it (top: {:?})",
        last.actions_dropped_top
    );
}
