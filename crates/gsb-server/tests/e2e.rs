//! End-to-end tests: an in-process gsb server on an ephemeral port and a
//! real client that authenticates, joins room 1, issues a move, and asserts
//! that it observes its entity's position change in the world snapshots.
//!
//! Every flow runs on BOTH transports (TCP and rUDP — `TransportKind`),
//! because the rUDP turn's acceptance criterion is "the existing e2e tests
//! pass on the new transport with the same intent". The `Client` enum is
//! the only place the transports differ:
//!
//! - TCP: length-prefixed frames over a per-connection socket; a server
//!   close is observable as EOF (read returns `Closed`).
//! - rUDP: one shared socket, a stateless cookie handshake at connect,
//!   a reliable control band (the client retransmits its own control
//!   frames; the server retransmits its own — visible in
//!   `UdpClientStats`), and a lossy snapshot band. UDP has no EOF: a
//!   closed session is observed as a failed `probe` (a heartbeat that
//!   never gets answered).

use std::time::{Duration, Instant};

use gsb_protocol::base::{Auth, AuthResult, Error, Heartbeat, HeartbeatAck, JoinRoom, JoinRoomResult};
use prost::Message;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

type Kind = gsb_server::TransportKind;

/// The one place the transports differ (see the module docs).
enum Client {
    Tcp(TcpStream),
    /// Boxed: `UdpClient` carries a 2 KB read buffer + queues; boxing
    /// keeps the enum's size at the small variant's (clippy's
    /// `large_enum_variant`).
    Udp(Box<gsb_net::udp::UdpClient>),
}

/// What a bounded wait for the next frame found.
enum Recv {
    Frame((u16, Vec<u8>)),
    /// TCP: the peer closed (EOF). rUDP: never (no EOF).
    Closed,
    /// No frame within the window (keep waiting).
    TimedOut,
}

impl Client {
    fn kind(&self) -> Kind {
        match self {
            Client::Tcp(_) => Kind::Tcp,
            Client::Udp(_) => Kind::Udp,
        }
    }

    async fn connect(kind: Kind, addr: std::net::SocketAddr) -> std::io::Result<Self> {
        match kind {
            Kind::Tcp => TcpStream::connect(addr)
                .await
                .map(Client::Tcp),
            Kind::Udp => gsb_net::udp::UdpClient::connect(addr)
                .await
                .map(|c| Client::Udp(Box::new(c))),
        }
    }

    /// Send one application frame (the wire encoding is transport-
    /// specific: length-prefixed body vs datagram kind).
    async fn write_frame(
        &mut self,
        op: u16,
        payload: &[u8],
    ) -> std::io::Result<()> {
        match self {
            Client::Tcp(stream) => {
                let body = 2 + payload.len();
                let mut out = Vec::with_capacity(4 + body);
                out.extend_from_slice(&(body as u32).to_le_bytes());
                out.extend_from_slice(&op.to_le_bytes());
                out.extend_from_slice(payload);
                stream.write_all(&out).await?;
                stream.flush().await
            }
            Client::Udp(c) => c.send_frame(op, payload.to_vec()).await,
        }
    }

    /// Wait up to `window` for the next frame.
    async fn recv(&mut self, window: Duration) -> std::io::Result<Recv> {
        match self {
            Client::Tcp(stream) => {
                match tokio::time::timeout(window, read_tcp_frame(stream)).await {
                    Ok(Some(f)) => Ok(Recv::Frame(f)),
                    Ok(None) => Ok(Recv::Closed),
                    Err(_) => Ok(Recv::TimedOut),
                }
            }
            Client::Udp(c) => match c.recv_frame(window).await? {
                Some(f) => Ok(Recv::Frame((f.op, f.payload.to_vec()))),
                None => Ok(Recv::TimedOut),
            },
        }
    }

    /// Liveness probe: a heartbeat; `true` if it gets an answer within
    /// the window. This is the rUDP notion of "the server closed" (UDP
    /// has no EOF): a session the server removed never answers.
    async fn probe(&mut self) -> std::io::Result<bool> {
        let hb = Heartbeat { tick: 0 }.encode_to_vec();
        self.write_frame(gsb_protocol::op::base::HEARTBEAT, &hb)
            .await?;
        let deadline = Instant::now() + Duration::from_millis(1500);
        loop {
            let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                return Ok(false);
            };
            match self.recv(remaining.min(Duration::from_millis(150))).await? {
                Recv::Frame((op, _)) if op == gsb_protocol::op::base::HEARTBEAT_ACK => {
                    return Ok(true)
                }
                Recv::Closed => return Ok(false),
                Recv::TimedOut | Recv::Frame(_) => {}
            }
        }
    }

    /// Assert (within `deadline`) that the connection is gone: EOF on
    /// TCP, a failed probe on rUDP.
    async fn assert_closed(&mut self, deadline: Instant) -> std::io::Result<()> {
        if matches!(self.kind(), Kind::Udp) {
            // Allow a beat for the close cascade to settle, then probe.
            tokio::time::sleep(Duration::from_millis(100)).await;
            if self.probe().await? {
                panic!("rUDP session still answers after the close");
            }
            return Ok(());
        }
        loop {
            let remaining = deadline
                .checked_duration_since(Instant::now())
                .unwrap_or_else(|| panic!("timed out waiting for EOF"));
            match self.recv(remaining).await? {
                Recv::Closed => return Ok(()),
                Recv::Frame(_) => {} // tolerate trailing frames before EOF
                Recv::TimedOut => {}
            }
        }
    }
}

/// TCP wire read: 4-byte LE length prefix + body. `None` = EOF/bad frame.
async fn read_tcp_frame<R: AsyncRead + Unpin>(stream: &mut R) -> Option<(u16, Vec<u8>)> {
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

/// A server config with the guardrail overrides a test needs, on a given
/// transport.
fn cfg_on(
    kind: Kind,
    idle_timeout_secs: Option<f64>,
    max_players: Option<u32>,
    max_connections: Option<u64>,
) -> gsb_server::Config {
    let mut cfg = gsb_server::Config {
        bind: "127.0.0.1:0".into(),
        room_count: 1,
        ..Default::default()
    };
    cfg.transport = kind;
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

fn kinds() -> [Kind; 2] {
    [Kind::Tcp, Kind::Udp]
}

/// The full control path (auth → join → action → snapshot) on a transport.
async fn join_and_observe_movement(kind: Kind) {
    let handle = gsb_server::start_server(cfg_on(kind, None, None, None))
        .await
        .expect("server starts");
    let mut client = Client::connect(kind, handle.addr)
        .await
        .expect("client connects");

    let auth = Auth { name: "e2e".into() }.encode_to_vec();
    client
        .write_frame(gsb_protocol::op::base::AUTH_REQ, &auth)
        .await
        .unwrap();
    let join = JoinRoom { room_id: 1 }.encode_to_vec();
    client
        .write_frame(gsb_protocol::op::base::JOIN_ROOM_REQ, &join)
        .await
        .unwrap();

    // The room ships self-contained world snapshots (one per change, plus
    // a low-rate keep-alive). Success = we first observe our entity, then
    // observe its position change (the move propagated: action → ingest →
    // movement system → snapshot → the transport's outbound path).
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut my_entity: u64 = 0;
    let mut move_sent = false;
    let mut first_pos: Option<(i32, i32)> = None;

    loop {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .unwrap_or_else(|| panic!("timed out (move_sent={move_sent})"));
        let (op, payload) = match client.recv(remaining).await.unwrap() {
            Recv::Frame(f) => f,
            Recv::Closed => panic!("server closed the connection"),
            Recv::TimedOut => {
                if Instant::now() >= deadline {
                    panic!("timed out waiting for frames");
                }
                continue;
            }
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
                if !move_sent {
                    let move_to = gsb_game::game::MoveTo { x: 10, y: 10, seq: 0 }.encode_to_vec();
                    client
                        .write_frame(gsb_game::op::MOVE_TO, &move_to)
                        .await
                        .unwrap();
                    move_sent = true;
                }
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

/// Session-lifecycle guardrail (item 1): a client that authenticates and
/// then says nothing is closed by the server on its own initiative — a
/// gentle `ERROR` (code 9) first, then teardown. On TCP that is EOF; on
/// rUDP (no FIN) it is the demux's idle deadline heap removing the
/// session — observed as a failed probe. Without the idle window this
/// connection (and its tasks/channels/registry entry) would sit until
/// process death: the half-open case.
async fn idle_connection_is_closed(kind: Kind) {
    let handle = gsb_server::start_server(cfg_on(kind, Some(1.0), None, None))
        .await
        .expect("server starts");
    let mut client = Client::connect(kind, handle.addr)
        .await
        .expect("client connects");
    let auth = Auth { name: "idle".into() }.encode_to_vec();
    client
        .write_frame(gsb_protocol::op::base::AUTH_REQ, &auth)
        .await
        .unwrap();

    // Stay silent. The deadline arms at the last inbound frame (the auth)
    // and fires after the 1 s window. Tolerate outbound frames (the room
    // keep-alive) — we are looking for the server's own close: ERROR 9,
    // then the connection being gone (EOF on TCP, a failed probe on rUDP).
    let deadline = Instant::now() + Duration::from_secs(8);
    let mut got_server_closed_error = false;
    while !got_server_closed_error {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .unwrap_or_else(|| {
                panic!("timed out (got_server_closed_error={got_server_closed_error})")
            });
        let (op, payload) = match client.recv(remaining).await.unwrap() {
            Recv::Frame(f) => f,
            Recv::Closed => break, // TCP: the close raced ahead of the ERROR (shouldn't)
            Recv::TimedOut => {
                if Instant::now() >= deadline {
                    panic!("timed out waiting for the server close");
                }
                continue;
            }
        };
        if op == gsb_protocol::op::base::ERROR {
            let m: Error = Error::decode(&payload[..]).unwrap();
            assert_eq!(m.code, 9, "server-initiated close is code 9: {m:?}");
            got_server_closed_error = true;
        }
    }
    assert!(
        got_server_closed_error,
        "the close must be a gentle ERROR 9 BEFORE the teardown, not a bare drop"
    );
    client.assert_closed(deadline).await.unwrap();
    handle.stop().await;
}

/// Session-lifecycle guardrail (item 1), positive side: a client that
/// keeps any inbound traffic flowing (here: heartbeats every 250 ms
/// against a 1 s window) is never touched — its window resets on every
/// frame (the TCP reader pump's clock; the rUDP demux deadline heap), and
/// it keeps getting its heartbeat acks for the whole run.
async fn active_heartbeat_survives(kind: Kind) {
    let handle = gsb_server::start_server(cfg_on(kind, Some(1.0), None, None))
        .await
        .expect("server starts");
    let mut client = Client::connect(kind, handle.addr)
        .await
        .expect("client connects");
    let auth = Auth { name: "active".into() }.encode_to_vec();
    client
        .write_frame(gsb_protocol::op::base::AUTH_REQ, &auth)
        .await
        .unwrap();

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
            let hb = Heartbeat { tick: acks }.encode_to_vec();
            client
                .write_frame(gsb_protocol::op::base::HEARTBEAT, &hb)
                .await
                .expect("socket alive");
        }
        match client.recv(Duration::from_millis(500)).await.unwrap() {
            Recv::Frame((op, payload)) => {
                if op == gsb_protocol::op::base::HEARTBEAT_ACK {
                    let _m: HeartbeatAck = HeartbeatAck::decode(&payload[..]).unwrap();
                    acks += 1;
                }
            }
            Recv::Closed => panic!("server closed an active connection"),
            Recv::TimedOut => {} // quiet window: no ack this round
        }
    }
    assert!(acks >= 3, "heartbeats were answered throughout: {acks} acks");
    handle.stop().await;
}

/// Capacity guardrail (item 2), gentle rejection: with `max_players = 1`,
/// the second joiner gets `ERROR` code 8 — and its connection STAYS ALIVE
/// (it can pick another room or retry; a silent close would masquerade as
/// a network failure and trigger reconnect storms against a busy server).
async fn room_full_gentle_rejection(kind: Kind) {
    let handle = gsb_server::start_server(cfg_on(kind, None, Some(1), None))
        .await
        .expect("server starts");
    let addr = handle.addr;

    // Player A takes the only seat.
    let mut a = Client::connect(kind, addr).await.expect("A connects");
    let auth_a = Auth { name: "a".into() }.encode_to_vec();
    a.write_frame(gsb_protocol::op::base::AUTH_REQ, &auth_a)
        .await
        .unwrap();
    let join = JoinRoom { room_id: 1 }.encode_to_vec();
    a.write_frame(gsb_protocol::op::base::JOIN_ROOM_REQ, &join)
        .await
        .unwrap();
    let mut a_joined = false;
    let deadline = Instant::now() + Duration::from_secs(5);
    while !a_joined {
        let (op, _payload) = match a.recv(deadline.saturating_duration_since(Instant::now())).await.unwrap() {
            Recv::Frame(f) => f,
            Recv::Closed => panic!("A's connection ended before the join"),
            Recv::TimedOut => {
                if Instant::now() >= deadline {
                    panic!("A did not join within 5 s");
                }
                continue;
            }
        };
        a_joined = op == gsb_protocol::op::base::JOIN_ROOM_RESULT;
    }

    // Player B is rejected (code 8)…
    let mut b = Client::connect(kind, addr).await.expect("B connects");
    let auth_b = Auth { name: "b".into() }.encode_to_vec();
    b.write_frame(gsb_protocol::op::base::AUTH_REQ, &auth_b)
        .await
        .unwrap();
    b.write_frame(gsb_protocol::op::base::JOIN_ROOM_REQ, &join)
        .await
        .unwrap();
    let mut b_rejected = false;
    let deadline = Instant::now() + Duration::from_secs(5);
    while !b_rejected {
        let (op, payload) = match b.recv(deadline.saturating_duration_since(Instant::now())).await.unwrap() {
            Recv::Frame(f) => f,
            Recv::Closed => panic!("B's connection ended before the rejection"),
            Recv::TimedOut => {
                if Instant::now() >= deadline {
                    panic!("B was not rejected within 5 s");
                }
                continue;
            }
        };
        if op == gsb_protocol::op::base::ERROR {
            let m: Error = Error::decode(&payload[..]).unwrap();
            assert_eq!(m.code, 8, "room-full rejection is code 8: {m:?}");
            b_rejected = true;
        }
    }

    // …and B stays alive: the rejection is gentle, not a drop.
    let hb = Heartbeat { tick: 7 }.encode_to_vec();
    b.write_frame(gsb_protocol::op::base::HEARTBEAT, &hb)
        .await
        .expect("B's connection still alive");
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut acked = false;
    while !acked {
        let (op, payload) = match b.recv(deadline.saturating_duration_since(Instant::now())).await.unwrap() {
            Recv::Frame(f) => f,
            Recv::Closed => panic!("B's connection was dropped after all"),
            Recv::TimedOut => {
                if Instant::now() >= deadline {
                    panic!("B did not get an ack after the gentle rejection");
                }
                continue;
            }
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
/// `ERROR` code 9 followed by teardown (this one IS a close: the registry
/// has no seat for it, so the connection cannot exist). The first
/// connection is unaffected.
async fn connection_capacity_rejects(kind: Kind) {
    let handle = gsb_server::start_server(cfg_on(kind, None, None, Some(1)))
        .await
        .expect("server starts");
    let addr = handle.addr;

    // A takes the only seat (and auths, so it is fully live).
    let mut a = Client::connect(kind, addr).await.expect("A connects");
    let auth_a = Auth { name: "a".into() }.encode_to_vec();
    a.write_frame(gsb_protocol::op::base::AUTH_REQ, &auth_a)
        .await
        .unwrap();

    // B is rejected at birth: ERROR 9, then teardown.
    let mut b = Client::connect(kind, addr).await.expect("B connects");
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut got9 = false;
    while !got9 {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .unwrap_or_else(|| panic!("timed out (got9={got9})"));
        let (op, payload) = match b.recv(remaining).await.unwrap() {
            Recv::Frame(f) => f,
            Recv::Closed => break,
            Recv::TimedOut => {
                if Instant::now() >= deadline {
                    panic!("timed out");
                }
                continue;
            }
        };
        if op == gsb_protocol::op::base::ERROR {
            let m: Error = Error::decode(&payload[..]).unwrap();
            assert_eq!(m.code, 9, "capacity rejection is code 9: {m:?}");
            got9 = true;
        }
    }
    assert!(got9, "B must see the capacity ERROR before the close");
    b.assert_closed(deadline).await.unwrap();

    // A is unaffected by B's rejection.
    let hb = Heartbeat { tick: 1 }.encode_to_vec();
    a.write_frame(gsb_protocol::op::base::HEARTBEAT, &hb)
        .await
        .expect("A still alive");
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut acked = false;
    while !acked {
        let (op, _payload) = match a.recv(deadline.saturating_duration_since(Instant::now())).await.unwrap() {
            Recv::Frame(f) => f,
            Recv::Closed => panic!("A's connection ended"),
            Recv::TimedOut => {
                if Instant::now() >= deadline {
                    panic!("A did not get an ack");
                }
                continue;
            }
        };
        acked = op == gsb_protocol::op::base::HEARTBEAT_ACK;
    }
    handle.stop().await;
}

/// Fairness guardrail (item 3), attribution: a single client that floods
/// MOVE_TO overflows ITS OWN bounded action channel — the room (bounded
/// pull) never drops, and the drops are counted per connection and surface
/// in the net scope attributed to the flooder (`actions_dropped_top`).
async fn flooder_drops_attributed(kind: Kind) {
    let mut cfg = gsb_server::Config {
        bind: "127.0.0.1:0".into(),
        room_count: 1,
        ..Default::default()
    };
    cfg.transport = kind;
    let (rep_tx, rep_rx) = tokio::sync::mpsc::unbounded_channel::<gsb_core::metrics::MetricReport>();
    let handle = gsb_server::start_server_metrics(cfg, rep_tx)
        .await
        .expect("server starts");

    let mut client = Client::connect(kind, handle.addr)
        .await
        .expect("client connects");
    let auth = Auth { name: "flood".into() }.encode_to_vec();
    client
        .write_frame(gsb_protocol::op::base::AUTH_REQ, &auth)
        .await
        .unwrap();
    let join = JoinRoom { room_id: 1 }.encode_to_vec();
    client
        .write_frame(gsb_protocol::op::base::JOIN_ROOM_REQ, &join)
        .await
        .unwrap();

    // Wait for the join, then start the flood.
    let mut joined = false;
    let deadline = Instant::now() + Duration::from_secs(5);
    while !joined {
        let (op, _payload) = match client.recv(deadline.saturating_duration_since(Instant::now())).await.unwrap() {
            Recv::Frame(f) => f,
            Recv::Closed => panic!("server closed the flooder before the join"),
            Recv::TimedOut => {
                if Instant::now() >= deadline {
                    panic!("the flooder never joined");
                }
                continue;
            }
        };
        joined = op == gsb_protocol::op::base::JOIN_ROOM_RESULT;
    }

    let flood_start = Instant::now();
    let fdeadline = flood_start + Duration::from_secs(3);
    // TCP: a DEDICATED tight-write task (no read pacing — interleaving
    // reads would slow the flood below the room's 480/s pull budget).
    // rUDP: the client is one task (the read and the send share the
    // socket), so the flood interleaves NON-BLOCKING read-drains; the
    // flood frames travel the lossy game band, so retransmit state never
    // gets in the way.
    let move_payload = gsb_game::game::MoveTo { x: 1, y: 1, seq: 0 }.encode_to_vec();
    match client {
        Client::Tcp(stream) => {
            let (mut r, mut w) = stream.into_split();
            let mf = {
                let body = 2 + move_payload.len();
                let mut out = Vec::with_capacity(4 + body);
                out.extend_from_slice(&(body as u32).to_le_bytes());
                out.extend_from_slice(&gsb_game::op::MOVE_TO.to_le_bytes());
                out.extend_from_slice(&move_payload);
                out
            };
            let flood = tokio::spawn(async move {
                while Instant::now() < fdeadline {
                    if w.write_all(&mf).await.is_err() {
                        break; // peer gone
                    }
                }
            });
            while flood_start.elapsed() < Duration::from_secs(3) {
                let _ = tokio::time::timeout(Duration::from_millis(200), read_tcp_frame(&mut r)).await;
            }
            flood.await.expect("flood task exits");
        }
        Client::Udp(mut c) => {
            while Instant::now() < fdeadline {
                c.send_frame(gsb_game::op::MOVE_TO, move_payload.clone())
                    .await
                    .unwrap();
                // Non-blocking drain (keep the outbound path moving):
                while c.recv_frame(Duration::ZERO).await.unwrap().is_some() {}
            }
        }
    }

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

/// Anti-amplification guardrail (violation budget): a client that sends
/// unknown base-band opcodes (hard violations) is answered with an `ERROR`
/// on the FIRST THREE only (diagnosis for the client developer), then the
/// funnel goes silent (the amplification is bounded), and when the
/// weighted lifetime budget is exhausted (4 hard violations × weight 4 =
/// 16) the server closes the connection — `ERROR` code 9 naming the
/// violation budget, then teardown. On TCP that is EOF; on rUDP a failed
/// probe.
async fn violation_budget_close(kind: Kind) {
    let handle = gsb_server::start_server(cfg_on(kind, None, None, None))
        .await
        .expect("server starts");
    let mut client = Client::connect(kind, handle.addr)
        .await
        .expect("client connects");
    // Twenty unknown base-band opcodes with an empty payload, back to
    // back: far past the budget (16) and the answer limit (3).
    for i in 0..20u16 {
        client.write_frame(42 + i, &[]).await.unwrap();
    }

    let mut answered = 0u32;
    let mut close_reason: Option<String> = None;
    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .unwrap_or_else(|| panic!("timed out (answered={answered})"));
        // A short recv window, not the whole remaining: on rUDP the close
        // has no EOF, so the loop must keep cycling (and probing) — one
        // long wait would sleep straight through the deadline.
        let (op, payload) = match client.recv(remaining.min(Duration::from_millis(500))).await.unwrap() {
            Recv::Frame(f) => f,
            Recv::Closed => break, // TCP EOF: the close
            Recv::TimedOut => {
                if Instant::now() >= deadline {
                    panic!("timed out waiting for the budget close");
                }
                // rUDP has no EOF: once the close ERROR is out (or all
                // the answers are in), a failed probe is the close.
                if (close_reason.is_some() || answered >= 3)
                    && !client.probe().await.unwrap()
                {
                    break;
                }
                continue;
            }
        };
        if op != gsb_protocol::op::base::ERROR {
            continue;
        }
        let m: Error = Error::decode(&payload[..]).unwrap();
        match m.code {
            1 => {
                answered += 1;
            }
            9 => close_reason = Some(m.message),
            c => panic!("unexpected error code {c} in the budget flow"),
        }
    }
    // The budget's exact shape: 3 answered violations, then the close —
    // no answers after the third, and no close without the answers.
    assert_eq!(answered, 3, "exactly the first 3 violations are answered");
    let reason = close_reason.expect("the close must be an ERROR 9");
    assert!(
        reason.contains("violation"),
        "the close must name the violation budget: {reason}"
    );
    if kind == Kind::Tcp {
        client.assert_closed(deadline).await.unwrap();
    }
    handle.stop().await;
}

// ── the test entry points: every flow, both transports ─────────────────

#[tokio::test]
async fn client_joins_and_receives_snapshots() {
    for kind in kinds() {
        join_and_observe_movement(kind).await;
    }
}

#[tokio::test]
async fn idle_connection_is_closed_by_the_server() {
    for kind in kinds() {
        idle_connection_is_closed(kind).await;
    }
}

#[tokio::test]
async fn active_heartbeat_survives_the_idle_window() {
    for kind in kinds() {
        active_heartbeat_survives(kind).await;
    }
}

#[tokio::test]
async fn room_full_returns_gentle_error_code_8() {
    for kind in kinds() {
        room_full_gentle_rejection(kind).await;
    }
}

#[tokio::test]
async fn connection_capacity_rejects_with_code_9() {
    for kind in kinds() {
        connection_capacity_rejects(kind).await;
    }
}

#[tokio::test]
async fn flooder_drops_are_attributed_to_the_flooder() {
    for kind in kinds() {
        flooder_drops_attributed(kind).await;
    }
}

#[tokio::test]
async fn violation_budget_answers_three_then_closes_the_connection() {
    for kind in kinds() {
        violation_budget_close(kind).await;
    }
}
