//! End-to-end tests: an in-process gsb server on an ephemeral port and a
//! real client that authenticates, joins room 1, issues a move, and asserts
//! that it observes its entity's position change in the world snapshots.
//!
//! Every flow runs on ALL THREE transports — plaintext TCP, rUDP, and
//! TCP-over-TLS (`TransportKind` + `Config::tls_cert/tls_key`) — because
//! each transport turn's acceptance criterion is "the existing e2e tests
//! pass on the new transport with the same intent" (docs/SECURITY.md §2:
//! `TlsTransport` must pass the SAME suite as plaintext tcp; the
//! parametrized idiom is the rUDP round's, extended by one arm). The
//! `Client` (a `gsb_client` connection) is the only place the
//! transports differ:
//!
//! - TCP: length-prefixed frames over a per-connection socket; a server
//!   close is observable as EOF (read returns `Closed`).
//! - rUDP: one shared socket, a stateless cookie handshake at connect,
//!   a reliable control band (the client retransmits its own control
//!   frames; the server retransmits its own — visible in
//!   `UdpClientStats`), and a lossy snapshot band. UDP has no EOF: a
//!   closed session is observed as a failed `probe` (a heartbeat that
//!   never gets answered).
//! - TLS: the TCP framing over a rustls stream verified against a
//!   runtime-minted CA (`common::mint_tls_pki`); EOF semantics are TCP's.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use gsb_client::Conn;
use gsb_client::session::{self, Credentials};
use gsb_protocol::base::{AuthResult, Error, Heartbeat, HeartbeatAck, JoinRoom, JoinRoomResult};
use prost::Message;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;

mod common;

/// The three transports a guardrail flow runs on. `Tcp`/`Udp` map onto
/// `gsb_server::TransportKind`; `Tls` is TCP plus the runtime-minted test
/// PKI (the server config gets the cert/key PEM paths, the client trusts
/// the CA DER). Clone is cheap (an `Arc` clone at most).
#[derive(Clone)]
enum Kind {
    Tcp,
    Udp,
    Tls(Arc<common::TlsPki>),
}

/// The one place the transports differ (see the module docs): a
/// `gsb_client` connection over the door `Kind` names.
struct Client(Conn);

/// A probe's heartbeat interval: about three a second, so one lands in
/// every one-second slot of the connection actor's ACK throttle.
const PROBE_EVERY: Duration = Duration::from_millis(300);
/// How long a probe waits for a live session's answer: the hang guard.
const ALIVE_GUARD: Duration = Duration::from_secs(10);
/// How long an rUDP session must stay silent to read as gone: two
/// throttle slots of heartbeats (and short of the reliable band's 5 s
/// no-ACK bound, past which the client itself reports the session dead).
const GONE_WINDOW: Duration = Duration::from_secs(2);
/// Heartbeat numbers per probe; each probe takes its own range, far above
/// the numbers the tests send by hand.
const PROBE_RANGE: u64 = 1000;
static NEXT_PROBE: AtomicU64 = AtomicU64::new(1_000_000);

/// What a bounded wait for the next frame found.
enum Recv {
    Frame((u16, Vec<u8>)),
    /// TCP: the peer closed (EOF). rUDP: never (no EOF).
    Closed,
    /// No frame within the window (keep waiting).
    TimedOut,
}

impl Client {
    /// Whether this wire is rUDP (the only transport without EOF — the
    /// close-detection branches below key off exactly this).
    fn is_udp(&self) -> bool {
        self.0.is_udp()
    }

    async fn connect(kind: Kind, addr: std::net::SocketAddr) -> std::io::Result<Self> {
        let conn = match kind {
            Kind::Tcp => gsb_client::connect::tcp(addr).await?,
            Kind::Udp => gsb_client::connect::udp(addr).await?,
            Kind::Tls(pki) => {
                let tcp = TcpStream::connect(addr).await?;
                let dns: rustls::pki_types::ServerName<'static> =
                    common::TLS_SERVER_NAME.try_into().expect("dns name");
                gsb_client::tls::connect(tcp, &common::tls_client_connector(&pki), dns).await?
            }
        };
        Ok(Client(conn))
    }

    /// Send one application frame (the wire encoding is transport-
    /// specific: length-prefixed body vs datagram kind).
    async fn write_frame(&mut self, op: u16, payload: &[u8]) -> std::io::Result<()> {
        self.0.send(op, payload).await
    }

    /// Wait up to `window` for the next frame. A stream door's read error
    /// or refused frame ends the stream like EOF (`Closed`); an rUDP
    /// socket error is the test's failure.
    async fn recv(&mut self, window: Duration) -> std::io::Result<Recv> {
        match self.0.recv(window).await {
            Ok(gsb_client::Recv::Frame(f)) => Ok(Recv::Frame((f.op, f.payload.to_vec()))),
            Ok(gsb_client::Recv::Closed) => Ok(Recv::Closed),
            Ok(gsb_client::Recv::Quiet) => Ok(Recv::TimedOut),
            Err(e) if self.is_udp() => Err(e),
            Err(_) => Ok(Recv::Closed),
        }
    }

    /// Liveness probe: `true` once the server answers one of THIS
    /// probe's heartbeats; `false` on a close, or when `window` passes
    /// unanswered. This is the rUDP notion of "the server closed" (UDP
    /// has no EOF): a session the server removed never answers.
    ///
    /// Not one heartbeat (BACKLOG F34, the F24 shape): the connection
    /// actor answers at most one a second (wall clock), so a single
    /// heartbeat landing within a second of the last answered one goes
    /// unanswered on a live session — under load, one sent "about a
    /// second later" does. Numbered heartbeats go out every
    /// [`PROBE_EVERY`] until one of them is acknowledged (an ACK to an
    /// earlier heartbeat is not the answer); `window` bounds only the
    /// wait for a session that is gone.
    async fn probe_within(&mut self, window: Duration) -> std::io::Result<bool> {
        let first = NEXT_PROBE.fetch_add(PROBE_RANGE, Ordering::Relaxed);
        let deadline = Instant::now() + window;
        let mut next = first;
        let mut resend = Instant::now();
        loop {
            let now = Instant::now();
            if now >= deadline {
                return Ok(false);
            }
            if now >= resend && next < first + PROBE_RANGE {
                let hb = session::heartbeat(next);
                self.write_frame(hb.op, &hb.payload).await?;
                next += 1;
                resend = now + PROBE_EVERY;
            }
            match self
                .recv((deadline - now).min(Duration::from_millis(150)))
                .await?
            {
                Recv::Frame((op, payload)) if op == gsb_protocol::op::base::HEARTBEAT_ACK => {
                    let tick = HeartbeatAck::decode(&payload[..]).expect("an ACK").tick;
                    if (first..next).contains(&tick) {
                        return Ok(true);
                    }
                }
                Recv::Closed => return Ok(false),
                Recv::TimedOut | Recv::Frame(_) => {}
            }
        }
    }

    /// The session is alive: some heartbeat of a probe is answered. The
    /// window is only the hang guard.
    async fn probe(&mut self) -> std::io::Result<bool> {
        self.probe_within(ALIVE_GUARD).await
    }

    /// The rUDP session is gone: no heartbeat of a probe is answered for
    /// [`GONE_WINDOW`] — re-sent every [`PROBE_EVERY`], so the 1/s ACK
    /// throttle cannot make a live session read as gone.
    async fn gone(&mut self) -> std::io::Result<bool> {
        Ok(!self.probe_within(GONE_WINDOW).await?)
    }

    /// Assert (within `deadline`) that the connection is gone: EOF on
    /// TCP and TLS, a failed probe on rUDP.
    async fn assert_closed(&mut self, deadline: Instant) -> std::io::Result<()> {
        if self.is_udp() {
            // Allow a beat for the close cascade to settle, then probe.
            tokio::time::sleep(Duration::from_millis(100)).await;
            if !self.gone().await? {
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

/// A server config with the guardrail overrides a test needs, on a given
/// transport. TLS is the TCP transport + the minted PKI's PEM paths (the
/// same config keys an operator would set).
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
    match &kind {
        Kind::Udp => cfg.transport = gsb_server::TransportKind::Udp,
        Kind::Tcp | Kind::Tls(_) => cfg.transport = gsb_server::TransportKind::Tcp,
    }
    if let Kind::Tls(pki) = &kind {
        cfg.tls_cert = pki.cert_pem_path.clone();
        cfg.tls_key = pki.key_pem_path.clone();
    }
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

/// All transports the parametrized flows must pass on. One PKI mint per
/// test entry point (rcgen is milliseconds; the flows share it by `Arc`).
fn kinds() -> Vec<Kind> {
    vec![
        Kind::Tcp,
        Kind::Udp,
        Kind::Tls(Arc::new(common::mint_tls_pki("e2e"))),
    ]
}

/// The full control path (auth → join → action → snapshot) on a transport.
async fn join_and_observe_movement(kind: Kind) {
    let handle = gsb_server::start_server(cfg_on(kind.clone(), None, None, None))
        .await
        .expect("server starts");
    let mut client = Client::connect(kind, handle.addr)
        .await
        .expect("client connects");

    let auth = auth_wire("e2e", &[]).1;
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
                    let move_to = gsb_demo::game::MoveTo {
                        x: 10,
                        y: 10,
                        seq: 0,
                    }
                    .encode_to_vec();
                    client
                        .write_frame(gsb_demo::op::MOVE_TO, &move_to)
                        .await
                        .unwrap();
                    move_sent = true;
                }
            }
            gsb_protocol::op::base::ERROR => {
                let m: Error = Error::decode(&payload[..]).unwrap();
                panic!("server error: code={} message={}", m.code, m.message);
            }
            gsb_demo::op::WORLD_SNAPSHOT => {
                let m: gsb_demo::game::WorldSnapshot =
                    gsb_demo::game::WorldSnapshot::decode(&payload[..]).unwrap();
                assert!(m.sequence > 0, "snapshot sequence must be monotonic (> 0)");
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
    let handle = gsb_server::start_server(cfg_on(kind.clone(), Some(1.0), None, None))
        .await
        .expect("server starts");
    let mut client = Client::connect(kind, handle.addr)
        .await
        .expect("client connects");
    let auth = auth_wire("idle", &[]).1;
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
///
/// It also pins the seam between that window and the §3.2 ACK throttle,
/// which is the thing most easily got wrong: the idle window is reset by
/// the ARRIVAL of a frame, never by the answer, so throttling answers can
/// never make a live client look idle. This client heartbeats four times
/// faster than the throttle answers and is still never closed — while the
/// ack count stays at the throttled rate, not the send rate.
async fn active_heartbeat_survives(kind: Kind) {
    let handle = gsb_server::start_server(cfg_on(kind.clone(), Some(1.0), None, None))
        .await
        .expect("server starts");
    let mut client = Client::connect(kind, handle.addr)
        .await
        .expect("client connects");
    let auth = auth_wire("active", &[]).1;
    client
        .write_frame(gsb_protocol::op::base::AUTH_REQ, &auth)
        .await
        .unwrap();

    // 5 s of 250 ms heartbeats against a 1 s window: 19 resets.
    let t0 = Instant::now();
    let mut acks = 0u64;
    let mut next_hb = t0;
    loop {
        let elapsed = t0.elapsed();
        if elapsed >= Duration::from_millis(5000) {
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
    assert!(
        acks >= 3,
        "heartbeats were answered throughout: {acks} acks"
    );
    // The §3.2 throttle's upper half: twenty heartbeats went out over the
    // five seconds, and at one answer per second only a handful can come
    // back. A 1:1 answer rate here would mean the post-auth throttle is
    // gone.
    assert!(
        acks <= 10,
        "post-auth heartbeat answers are throttled to ~1/s, not 1:1 with \
         the 4/s send rate: {acks} acks"
    );
    handle.stop().await;
}

/// Capacity guardrail (item 2), gentle rejection: with `max_players = 1`,
/// the second joiner gets `ERROR` code 8 — and its connection STAYS ALIVE
/// (it can pick another room or retry; a silent close would masquerade as
/// a network failure and trigger reconnect storms against a busy server).
async fn room_full_gentle_rejection(kind: Kind) {
    let handle = gsb_server::start_server(cfg_on(kind.clone(), None, Some(1), None))
        .await
        .expect("server starts");
    let addr = handle.addr;

    // Player A takes the only seat.
    let mut a = Client::connect(kind.clone(), addr)
        .await
        .expect("A connects");
    let auth_a = auth_wire("a", &[]).1;
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
        let (op, _payload) = match a
            .recv(deadline.saturating_duration_since(Instant::now()))
            .await
            .unwrap()
        {
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
    let auth_b = auth_wire("b", &[]).1;
    b.write_frame(gsb_protocol::op::base::AUTH_REQ, &auth_b)
        .await
        .unwrap();
    b.write_frame(gsb_protocol::op::base::JOIN_ROOM_REQ, &join)
        .await
        .unwrap();
    let mut b_rejected = false;
    let deadline = Instant::now() + Duration::from_secs(5);
    while !b_rejected {
        let (op, payload) = match b
            .recv(deadline.saturating_duration_since(Instant::now()))
            .await
            .unwrap()
        {
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
        let (op, payload) = match b
            .recv(deadline.saturating_duration_since(Instant::now()))
            .await
            .unwrap()
        {
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
    let handle = gsb_server::start_server(cfg_on(kind.clone(), None, None, Some(1)))
        .await
        .expect("server starts");
    let addr = handle.addr;

    // A takes the only seat (and auths, so it is fully live).
    let mut a = Client::connect(kind.clone(), addr)
        .await
        .expect("A connects");
    let auth_a = auth_wire("a", &[]).1;
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
        let (op, _payload) = match a
            .recv(deadline.saturating_duration_since(Instant::now()))
            .await
            .unwrap()
        {
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
    // Same config shape as every other flow (cfg_on picks TCP+PEM paths
    // for the TLS arm — identical to what an operator would write).
    let cfg = cfg_on(kind.clone(), None, None, None);
    let (rep_tx, rep_rx) =
        tokio::sync::mpsc::unbounded_channel::<gsb_core::metrics::MetricReport>();
    let handle = gsb_server::start_server_metrics(cfg, rep_tx)
        .await
        .expect("server starts");

    let mut client = Client::connect(kind, handle.addr)
        .await
        .expect("client connects");
    let auth = auth_wire("flood", &[]).1;
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
        let (op, _payload) = match client
            .recv(deadline.saturating_duration_since(Instant::now()))
            .await
            .unwrap()
        {
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
    // TCP/TLS: a DEDICATED tight-write task (no read pacing — interleaving
    // reads would slow the flood below the room's 480/s pull budget); the
    // rustls stream splits exactly like the TCP socket does.
    // rUDP: the client is one task (the read and the send share the
    // socket), so the flood interleaves NON-BLOCKING read-drains; the
    // flood frames travel the lossy game band, so retransmit state never
    // gets in the way.
    let move_payload = gsb_demo::game::MoveTo { x: 1, y: 1, seq: 0 }.encode_to_vec();
    match client.0.into_split() {
        Ok((mut r, mut w)) => {
            let mf = gsb_client::frame::encode(gsb_demo::op::MOVE_TO, &move_payload);
            let flood = tokio::spawn(async move {
                while Instant::now() < fdeadline {
                    if w.get_mut().write_all(&mf).await.is_err() {
                        break; // peer gone
                    }
                }
            });
            while flood_start.elapsed() < Duration::from_secs(3) {
                let _ = tokio::time::timeout(Duration::from_millis(200), r.next()).await;
            }
            flood.await.expect("flood task exits");
        }
        Err(udp) => {
            let Conn::Udp(mut c) = *udp else {
                unreachable!("only rUDP has no halves")
            };
            while Instant::now() < fdeadline {
                c.send_frame(gsb_demo::op::MOVE_TO, move_payload.clone())
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
    // Attribution: the last report taken while the flooder was still on
    // the wire names it as the sole dropper, with every drop so far
    // accounted to it alone. (Since the table-pruning round, a CLOSED
    // connection's per-connection entry retires into the cumulative net
    // total — and `handle.stop` closes the flooder before the final
    // report — so the final report may list no one; the retirement
    // itself is locked by gsb-core's accumulator unit tests.)
    let live = reports
        .iter()
        .rev()
        .find(|r| !r.actions_dropped_top.is_empty())
        .expect("at least one report caught the flooder live");
    assert_eq!(
        live.actions_dropped_top.first(),
        Some(&(gsb_core::id::ConnectionId(1), live.net.actions_dropped)),
        "the sole connection (id 1) is the sole dropper: every drop is \
         attributed to it (top: {:?})",
        live.actions_dropped_top
    );
    assert!(
        last.net.actions_dropped >= live.net.actions_dropped,
        "the cumulative drop total is monotonic across the close"
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
    let handle = gsb_server::start_server(cfg_on(kind.clone(), None, None, None))
        .await
        .expect("server starts");
    let mut client = Client::connect(kind.clone(), handle.addr)
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
        let (op, payload) = match client
            .recv(remaining.min(Duration::from_millis(500)))
            .await
            .unwrap()
        {
            Recv::Frame(f) => f,
            Recv::Closed => break, // TCP EOF: the close
            Recv::TimedOut => {
                if Instant::now() >= deadline {
                    panic!("timed out waiting for the budget close");
                }
                // rUDP has no EOF: once the close ERROR is out (or all
                // the answers are in), a failed probe is the close.
                if (close_reason.is_some() || answered >= 3) && client.gone().await.unwrap() {
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
    // TCP and TLS both deliver the close as EOF; rUDP is probe-based.
    if !matches!(kind, Kind::Udp) {
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

// ── the control-plane entry (feature A) + the RPC pattern (feature B),
//    over the wire ───────────────────────────────────────────────────────
//
// These flows are wire-level and TCP-only by intent: rUDP's lossy snapshot
// band has no place in byte-exact control-frame assertions, and the
// transport-agnostic contracts are locked by the gsb-core suites
// (control_plane.rs, rpc.rs, ticket.rs) and by the per-transport tests
// above. The `Client` helper is reused as-is.

fn room_cfg(id: u64) -> gsb_core::room::RoomConfig {
    gsb_core::room::RoomConfig {
        id: gsb_core::id::RoomId(id),
        tick_hz: 30.0, // the server's global rate (the room must divide it)
        ..Default::default()
    }
}

fn auth_wire(name: &str, ticket: &[u8]) -> (u16, Vec<u8>) {
    let f = session::auth_req(&Credentials::named(name).with_ticket(ticket));
    (f.op, f.payload.to_vec())
}

fn join_wire(room: u64) -> (u16, Vec<u8>) {
    let f = session::join_req(room);
    (f.op, f.payload.to_vec())
}

/// One correlated request on the wire: the base-band envelope opcode
/// carrying `{id, op, payload}` (the room decodes the envelope).
fn rpc_wire(id: u64, inner_op: u16, payload: &[u8]) -> (u16, Vec<u8>) {
    (
        gsb_protocol::op::base::RPC_REQ,
        gsb_protocol::base::RpcRequest {
            id,
            op: inner_op as u32,
            payload: payload.to_vec(),
        }
        .encode_to_vec(),
    )
}

/// Auth + join `room` over a client; returns the entity id.
///
/// The per-call unique name is load-bearing since reconnect landed: the
/// name IS the resume key, and a second LIVE session with one identity
/// supersedes (ERROR 9-closes) the first by design.
async fn auth_and_join(client: &mut Client, room: u64) -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let name = format!("e2e-{}", SEQ.fetch_add(1, Ordering::SeqCst));
    let creds = Credentials::named(name);
    // Snapshots may race the join result: skipped.
    match session::auth_and_join(&mut client.0, &creds, room, Duration::from_secs(10), |_| {}).await
    {
        Ok(j) => {
            assert!(j.entity != 0);
            j.entity
        }
        Err(gsb_client::ClientError::Server(m)) => {
            panic!("join failed: code={} message={}", m.raw, m.message)
        }
        Err(gsb_client::ClientError::AuthRefused(_)) => panic!("auth must succeed"),
        Err(gsb_client::ClientError::TimedOut) => panic!("timed out in auth_and_join"),
        Err(e) => panic!("server closed the connection: {e}"),
    }
}

/// The control plane's lifecycle API: create is idempotent (the same
/// config twice → one room; a different config → conflict), close is a
/// no-op on an absent room, and status reports the registry's view.
#[tokio::test]
async fn control_plane_room_lifecycle_is_idempotent_over_the_wire() {
    let handle = gsb_server::start_server(cfg_on(Kind::Tcp, None, None, None))
        .await
        .expect("server starts");

    // "Open the room" twice with the same config: two Ok replies, ONE
    // room (the second reply is the existing room's status).
    let cfg7 = room_cfg(7);
    let s1 = handle.open_room(cfg7.clone()).await.expect("first open");
    assert_eq!(s1, gsb_core::registry::RoomStatus::Running { members: 0 });
    let s2 = handle
        .open_room(cfg7.clone())
        .await
        .expect("second open (retry)");
    assert_eq!(
        s2,
        gsb_core::registry::RoomStatus::Running { members: 0 },
        "the idempotent retry must report the same room"
    );

    // A different config for a live room conflicts (the typo guard).
    let conflict = gsb_core::room::RoomConfig {
        max_players: Some(1),
        ..cfg7.clone()
    };
    assert!(
        matches!(
            handle.open_room(conflict).await,
            Err(gsb_core::error::CoreError::RoomConflict(7))
        ),
        "a different config must conflict, not replace"
    );

    // Status: running, then the close lifecycle (destroyed → absent →
    // an idempotent close no-op).
    assert_eq!(
        handle
            .room_status(gsb_core::id::RoomId(7))
            .await
            .expect("status"),
        gsb_core::registry::RoomStatus::Running { members: 0 }
    );
    assert_eq!(
        handle
            .close_room(gsb_core::id::RoomId(7))
            .await
            .expect("close"),
        gsb_core::registry::RoomStatus::Destroyed
    );
    assert_eq!(
        handle
            .room_status(gsb_core::id::RoomId(7))
            .await
            .expect("status"),
        gsb_core::registry::RoomStatus::Absent
    );
    assert_eq!(
        handle
            .close_room(gsb_core::id::RoomId(7))
            .await
            .expect("close no-op"),
        gsb_core::registry::RoomStatus::Absent
    );

    handle.stop().await;
}

/// A runtime-opened room enforces its own capacity: a second player into
/// a max-players-1 room gets the gentle ERROR 8 (the connection stays
/// alive).
#[tokio::test]
async fn control_plane_runtime_room_capacity_is_enforced() {
    let handle = gsb_server::start_server(cfg_on(Kind::Tcp, None, None, None))
        .await
        .expect("server starts");
    let cfg5 = gsb_core::room::RoomConfig {
        max_players: Some(1),
        ..room_cfg(5)
    };
    handle.open_room(cfg5).await.expect("open room 5");

    let mut a = Client::connect(Kind::Tcp, handle.addr).await.expect("A");
    auth_and_join(&mut a, 5).await;

    let mut b = Client::connect(Kind::Tcp, handle.addr).await.expect("B");
    let (op, payload) = auth_wire("e2e-cap-b", &[]);
    b.write_frame(op, &payload).await.unwrap();
    let (op, payload) = join_wire(5);
    b.write_frame(op, &payload).await.unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .unwrap_or_else(|| panic!("timed out waiting for B's rejection"));
        match b.recv(remaining).await.unwrap() {
            Recv::Frame((op, payload)) => {
                if op != gsb_protocol::op::base::ERROR {
                    continue;
                }
                let m = Error::decode(&payload[..]).unwrap();
                assert_eq!(m.code, 8, "a full room is a gentle code 8");
                // B's connection stays alive (the gentle-reject contract):
                assert!(b.probe().await.unwrap(), "B must survive the code 8");
                return;
            }
            Recv::Closed => panic!("B was closed instead of gently rejected"),
            Recv::TimedOut => {}
        }
    }
}

/// The match-result exit seam over the wire: a room with a player in it
/// reports its final state through `handle.match_results` on close.
#[tokio::test]
async fn control_plane_match_result_reports_on_close() {
    let handle = gsb_server::start_server(cfg_on(Kind::Tcp, None, None, None))
        .await
        .expect("server starts");
    let mut a = Client::connect(Kind::Tcp, handle.addr)
        .await
        .expect("client");
    auth_and_join(&mut a, 1).await;

    assert_eq!(
        handle
            .close_room(gsb_core::id::RoomId(1))
            .await
            .expect("close"),
        gsb_core::registry::RoomStatus::Destroyed
    );
    let mut handle = handle;
    let result = tokio::time::timeout(Duration::from_secs(10), handle.match_results.recv())
        .await
        .expect("timed out waiting for the match result")
        .expect("result sink closed");
    assert_eq!(result.room, gsb_core::id::RoomId(1));
    // The demo's result is its final world snapshot (self-contained,
    // delta=false) and it carries the player that was in the room.
    let snap = gsb_demo::game::WorldSnapshot::decode(&result.payload[..])
        .expect("the result is a WorldSnapshot");
    assert!(!snap.delta, "the shutdown snapshot is a full");
    assert!(
        !snap.entities.is_empty(),
        "the result must carry the room's final players"
    );
    handle.stop().await;
}

/// The RPC pattern over the wire: a room-local request is answered
/// (the `Private.responses` shape, to the requester only — no leak to
/// the other player in the room), and an external-I/O request is
/// answered on a LATER tick (its answer cannot ride the request's own
/// tick's frames).
#[tokio::test]
async fn rpc_over_the_wire_local_no_leak_external_later_tick() {
    let handle = gsb_server::start_server(cfg_on(Kind::Tcp, None, None, None))
        .await
        .expect("server starts");
    let mut a = Client::connect(Kind::Tcp, handle.addr).await.expect("A");
    let mut b = Client::connect(Kind::Tcp, handle.addr).await.expect("B");
    let a_entity = auth_and_join(&mut a, 1).await;
    auth_and_join(&mut b, 1).await;

    // A's current position (from its own snapshot) — the ability will
    // target IT, so the demo's range check (10 units) trivially passes.
    let deadline = Instant::now() + Duration::from_secs(10);
    // Uninitialized on purpose: the loop's only exits are the
    // assignment below (break) or a panic — a single write before
    // use, so no `mut` is needed.
    let a_pos: (i32, i32);
    loop {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .unwrap_or_else(|| panic!("timed out waiting for A's snapshot"));
        match a.recv(remaining).await.unwrap() {
            Recv::Frame((op, payload)) if op == gsb_demo::op::WORLD_SNAPSHOT => {
                let m = gsb_demo::game::WorldSnapshot::decode(&payload[..]).unwrap();
                if let Some(rec) = m.entities.iter().find(|e| e.entity == a_entity) {
                    a_pos = (rec.x, rec.y);
                    break;
                }
            }
            Recv::Frame(_) => {}
            Recv::Closed => panic!("A closed"),
            Recv::TimedOut => {}
        }
    }

    // --- Room-local (ABILITY): request → answer, and B never sees it.
    let use_msg = gsb_demo::game::AbilityUse {
        x: a_pos.0,
        y: a_pos.1,
    }
    .encode_to_vec();
    let (op, payload) = rpc_wire(1, gsb_demo::op::ABILITY, &use_msg);
    a.write_frame(op, &payload).await.unwrap();

    // A: read until the reply for id 1 arrives; remember the snapshot
    // sequence stream (the reply rides the request's ingest tick).
    let mut a_reply: Option<(bool, u16)> = None;
    let deadline = Instant::now() + Duration::from_secs(10);
    while a_reply.is_none() && Instant::now() < deadline {
        match a.recv(Duration::from_millis(200)).await.unwrap() {
            Recv::Frame((op, payload)) if op == gsb_demo::op::PRIVATE => {
                let m = gsb_demo::game::Private::decode(&payload[..]).unwrap();
                for r in &m.responses {
                    if r.id == 1 {
                        a_reply = Some((r.ok, r.op as u16));
                    }
                }
            }
            Recv::Frame(_) => {}
            Recv::Closed => panic!("A closed"),
            Recv::TimedOut => {}
        }
    }
    let (ok, op) = a_reply.expect("A's local request must be answered");
    assert!(ok, "an in-range ability must succeed");
    assert_eq!(op, gsb_demo::op::ABILITY, "the answer carries the inner op");

    // B: drain its stream for a window; it must NOT carry a reply for
    // A's id (the answer is per-connection private).
    let deadline = Instant::now() + Duration::from_millis(250);
    while Instant::now() < deadline {
        match b.recv(Duration::from_millis(50)).await.unwrap() {
            Recv::Frame((op, payload)) if op == gsb_demo::op::PRIVATE => {
                let m = gsb_demo::game::Private::decode(&payload[..]).unwrap();
                assert!(
                    !m.responses.iter().any(|r| r.id == 1),
                    "A's reply leaked to B"
                );
            }
            Recv::Frame(_) => {}
            Recv::Closed => panic!("B closed"),
            Recv::TimedOut => break,
        }
    }

    // --- External-I/O (ECONOMY): the round trip over the wire.
    let buy = gsb_demo::game::BuyItem {
        kind: "potion".into(),
    }
    .encode_to_vec();
    let (op, payload) = rpc_wire(2, gsb_demo::op::ECONOMY, &buy);
    a.write_frame(op, &payload).await.unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut answered: Option<Vec<u8>> = None;
    while answered.is_none() && Instant::now() < deadline {
        match a.recv(Duration::from_millis(200)).await.unwrap() {
            Recv::Frame((op, payload)) if op == gsb_demo::op::PRIVATE => {
                let m = gsb_demo::game::Private::decode(&payload[..]).unwrap();
                for r in &m.responses {
                    if r.id == 2 {
                        assert!(r.ok, "buying a potion must succeed");
                        assert_eq!(r.op as u16, gsb_demo::op::ECONOMY);
                        answered = Some(r.payload.clone());
                    }
                }
            }
            Recv::Frame(_) => {}
            Recv::Closed => panic!("A closed"),
            Recv::TimedOut => {}
        }
    }
    let result = gsb_demo::game::BuyResult::decode(
        &answered.expect("the external request must be answered")[..],
    )
    .expect("the answer decodes as BuyResult");
    assert!(result.ok, "the economy service must sell the potion");
    assert_eq!(result.price, 100, "the price comes from the service");

    // --- Later-tick proof over the wire: send the EXTERNAL request
    //     first and the LOCAL one in the same burst. If both are
    //     ingested in one tick (the normal case), the local answer
    //     rides THAT tick's broadcast while the external answer can
    //     only ride a later one — so the local reply precedes the
    //     external reply in A's stream. An inverted order means the
    //     two writes straddled a tick boundary (a scheduling race, not
    //     a protocol fact) — the pair is replayed with fresh ids.
    let buy2 = gsb_demo::game::BuyItem {
        kind: "potion".into(),
    }
    .encode_to_vec();
    let use2 = gsb_demo::game::AbilityUse {
        x: a_pos.0,
        y: a_pos.1,
    }
    .encode_to_vec();
    let mut settled = false;
    for attempt in 0..8 {
        let ext_id = 100 + attempt;
        let loc_id = 101 + attempt;
        let (op, payload) = rpc_wire(ext_id, gsb_demo::op::ECONOMY, &buy2);
        a.write_frame(op, &payload).await.unwrap();
        let (op, payload) = rpc_wire(loc_id, gsb_demo::op::ABILITY, &use2);
        a.write_frame(op, &payload).await.unwrap();

        let mut local_first: Option<bool> = None;
        let mut ext_seen = false;
        let mut loc_seen = false;
        let deadline = Instant::now() + Duration::from_secs(10);
        while !(ext_seen && loc_seen) && Instant::now() < deadline {
            match a.recv(Duration::from_millis(200)).await.unwrap() {
                Recv::Frame((op, payload)) if op == gsb_demo::op::PRIVATE => {
                    let m = gsb_demo::game::Private::decode(&payload[..]).unwrap();
                    for r in &m.responses {
                        if r.id == loc_id {
                            if !ext_seen {
                                local_first = Some(true);
                            }
                            loc_seen = true;
                        } else if r.id == ext_id {
                            if !loc_seen {
                                local_first = Some(false);
                            }
                            ext_seen = true;
                        }
                    }
                }
                Recv::Frame(_) => {}
                Recv::Closed => panic!("A closed"),
                Recv::TimedOut => {}
            }
        }
        assert!(
            ext_seen && loc_seen,
            "attempt {attempt}: both probe answers must arrive (ext_seen={ext_seen}, loc_seen={loc_seen})"
        );
        if local_first.expect("both seen") {
            settled = true;
            break; // the local answer rode the ingest tick; the
            // external one necessarily rode a later one.
        }
        // Straddled tick boundary: this pair proves nothing about
        // ordering — replay with fresh ids.
    }
    assert!(
        settled,
        "the local answer must precede the external one (8 straddled
         attempts is not a scheduling race)"
    );

    handle.stop().await;
}

/// The ticket-validation hook over the wire (feature A, item 2): an
/// invalid ticket is a normal rejection (code 10, the connection
/// survives); a valid ticket authenticates with the hook's identity and
/// pins the join (wrong room → code 11, right room → in); and while a
/// slow validation is in flight, the room keeps ticking (the tick body
/// never awaits the validator — the in-room player's snapshot stream
/// stays continuous).
///
/// "In flight" is a condition, not a window (BACKLOG F25): the slow
/// validation parks until the test releases it, and A's snapshots are
/// counted between the validator's entry and that release. (A 250 ms
/// sleep and a 350 ms wall-clock window held only while the machine
/// kept the room at rate.)
#[tokio::test]
async fn ticket_hook_flow_and_slow_auth_keeps_the_tick_running() {
    // The platform's validator (the base defines the hook, the platform
    // ships this behaviour): `slow` reports its entry and parks until
    // released (a signature-service round trip that takes as long as the
    // test needs), `good` resolves fast.
    let (entered_tx, mut entered) = tokio::sync::mpsc::unbounded_channel::<()>();
    let release = std::sync::Arc::new(tokio::sync::Notify::new());
    let gate = std::sync::Arc::clone(&release);
    let validator: gsb_core::auth::TicketValidator = std::sync::Arc::new(move |t: bytes::Bytes| {
        let (entered_tx, gate) = (entered_tx.clone(), std::sync::Arc::clone(&gate));
        Box::pin(async move {
            match t.as_ref() {
                b"slow" => {
                    let _ = entered_tx.send(());
                    gate.notified().await;
                    Ok(gsb_core::auth::ValidatedTicket {
                        player: "slow".into(),
                        room: gsb_core::id::RoomId(1),
                    })
                }
                b"good" => Ok(gsb_core::auth::ValidatedTicket {
                    player: "neo".into(),
                    room: gsb_core::id::RoomId(1),
                }),
                _ => Err(gsb_core::auth::TicketError::Rejected("bad ticket".into())),
            }
        })
    });
    let hooks = gsb_server::ServerHooks {
        ticket: Some(gsb_core::auth::TicketAuth {
            validator,
            // Far past the parked validation: the release, not the hook's
            // deadline, ends it.
            timeout: Duration::from_secs(60),
        }),
    };
    let handle = gsb_server::start_server_with(cfg_on(Kind::Tcp, None, None, None), hooks)
        .await
        .expect("server starts with the ticket hook");

    let mut a = Client::connect(Kind::Tcp, handle.addr).await.expect("A");
    // An invalid ticket: code 10 (normal rejection), and A SURVIVES.
    let (op, payload) = auth_wire("a", b"bogus");
    a.write_frame(op, &payload).await.unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .unwrap_or_else(|| panic!("timed out waiting for the rejection"));
        match a.recv(remaining).await.unwrap() {
            Recv::Frame((op, payload)) if op == gsb_protocol::op::base::ERROR => {
                let m = Error::decode(&payload[..]).unwrap();
                assert_eq!(m.code, 10, "an invalid ticket is a normal rejection");
                break;
            }
            Recv::Frame(_) => {}
            Recv::Closed => panic!("an invalid ticket must not close the connection"),
            Recv::TimedOut => {}
        }
    }
    assert!(
        a.probe().await.unwrap(),
        "A must survive the ticket rejection"
    );

    // A valid ticket: the hook's identity (player + pinned room).
    let (op, payload) = auth_wire("a", b"good");
    a.write_frame(op, &payload).await.unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .unwrap_or_else(|| panic!("timed out waiting for the auth result"));
        match a.recv(remaining).await.unwrap() {
            Recv::Frame((op, payload)) if op == gsb_protocol::op::base::AUTH_RESULT => {
                let m = AuthResult::decode(&payload[..]).unwrap();
                assert!(m.ok);
                assert_eq!(m.player, "neo", "the hook's identity wins");
                assert_eq!(m.room, 1, "the ticket pins the room");
                break;
            }
            Recv::Frame(_) => {}
            Recv::Closed => panic!("closed during auth"),
            Recv::TimedOut => {}
        }
    }
    // The pin: the wrong room is a normal rejection (code 11)…
    let (op, payload) = join_wire(2);
    a.write_frame(op, &payload).await.unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .unwrap_or_else(|| panic!("timed out waiting for the pin rejection"));
        match a.recv(remaining).await.unwrap() {
            Recv::Frame((op, payload)) if op == gsb_protocol::op::base::ERROR => {
                let m = Error::decode(&payload[..]).unwrap();
                assert_eq!(m.code, 11, "a non-pinned join is a normal rejection");
                break;
            }
            Recv::Frame(_) => {}
            Recv::Closed => panic!("a pin rejection must not close the connection"),
            Recv::TimedOut => {}
        }
    }
    // …the pinned room is accepted (a raw join — A is already authed).
    let (op, payload) = join_wire(1);
    a.write_frame(op, &payload).await.unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .unwrap_or_else(|| panic!("timed out waiting for the join result"));
        match a.recv(remaining).await.unwrap() {
            Recv::Frame((op, payload)) if op == gsb_protocol::op::base::JOIN_ROOM_RESULT => {
                let m = JoinRoomResult::decode(&payload[..]).unwrap();
                assert!(m.entity != 0);
                break;
            }
            Recv::Frame(_) => {}
            Recv::Closed => panic!("closed during the pinned join"),
            Recv::TimedOut => {}
        }
    }

    // Continuous movement: every tick ships a snapshot for A.
    let move_to = gsb_demo::game::MoveTo {
        x: 50,
        y: 50,
        seq: 0,
    }
    .encode_to_vec();
    a.write_frame(gsb_demo::op::MOVE_TO, &move_to)
        .await
        .unwrap();

    // Now B authenticates with the SLOW ticket. While B's actor is parked
    // on the validator, A's snapshot stream must keep running — the
    // room's tick body never awaits a connection's validation (the
    // ticket is off the tick path).
    let mut b = Client::connect(Kind::Tcp, handle.addr).await.expect("B");
    let (op, payload) = auth_wire("b", b"slow");
    b.write_frame(op, &payload).await.unwrap();
    tokio::time::timeout(Duration::from_secs(10), entered.recv())
        .await
        .expect("B's validation started")
        .expect("the validator is alive");
    // Snapshots the room made AFTER the validation began. What already
    // sits on A's socket may be older, so a fence first: A's connection
    // answers a heartbeat sent from here on through the same outbound
    // queue the room's snapshot batches take, so every snapshot behind
    // that HEARTBEAT_ACK was queued after it. (A's probe above already
    // took its ACK; the 1/s ACK throttle may skip a heartbeat, so it is
    // re-sent until one is answered — and only the fence's own number
    // counts: a late ACK to one of the probe's heartbeats was queued
    // before the validation began.)
    let mut fenced = false;
    let mut next_heartbeat = Instant::now();
    let mut snapshots_in_flight: Vec<u64> = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(10);
    while snapshots_in_flight.len() < 3 {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .unwrap_or_else(|| {
                panic!(
                    "A must keep receiving snapshots during B's validation \
                 (the tick body stays synchronous; fenced: {fenced}, saw \
                 {snapshots_in_flight:?})"
                )
            });
        if !fenced && Instant::now() >= next_heartbeat {
            let hb = session::heartbeat(1);
            a.write_frame(hb.op, &hb.payload).await.unwrap();
            next_heartbeat = Instant::now() + Duration::from_millis(1_100);
        }
        match a
            .recv(remaining.min(Duration::from_millis(100)))
            .await
            .unwrap()
        {
            Recv::Frame((op, payload)) if op == gsb_protocol::op::base::HEARTBEAT_ACK => {
                if HeartbeatAck::decode(&payload[..]).expect("an ACK").tick == 1 {
                    fenced = true;
                }
            }
            Recv::Frame((op, payload)) if fenced && op == gsb_demo::op::WORLD_SNAPSHOT => {
                let m = gsb_demo::game::WorldSnapshot::decode(&payload[..]).unwrap();
                snapshots_in_flight.push(m.sequence);
            }
            Recv::Frame(_) | Recv::TimedOut => {}
            Recv::Closed => panic!("A closed during the slow-auth validation"),
        }
    }
    // Strictly increasing: the room kept stepping (no stall).
    let increasing = snapshots_in_flight.windows(2).all(|w| w[1] > w[0]);
    assert!(
        increasing,
        "snapshot sequences must be strictly increasing: {snapshots_in_flight:?}"
    );
    // Still in flight: nothing has answered B yet.
    assert!(
        matches!(
            b.recv(Duration::from_millis(1)).await.unwrap(),
            Recv::TimedOut
        ),
        "B's validation was still parked while A's stream ran"
    );
    release.notify_one();

    // B's slow validation completes once released.
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .unwrap_or_else(|| panic!("timed out waiting for B's auth result"));
        match b.recv(remaining).await.unwrap() {
            Recv::Frame((op, payload)) if op == gsb_protocol::op::base::AUTH_RESULT => {
                let m = AuthResult::decode(&payload[..]).unwrap();
                assert!(m.ok, "the slow ticket must succeed (it finished in time)");
                assert_eq!(m.player, "slow");
                break;
            }
            Recv::Frame(_) => {}
            Recv::Closed => panic!("B closed"),
            Recv::TimedOut => {}
        }
    }
    handle.stop().await;
}

// ── the reconnect feature end-to-end (RECONNECT §15 Tur B) ─────────────
//
// THE user-visible proof of the whole feature, over a REAL socket: a
// client whose transport dies WITHOUT a leave parks its entity (the
// demo rooms' MOBA-style hold), keeps being visible to the other
// member, and its NEXT session under the SAME identity resumes onto the
// LIVE entity with the SAME wire id, inputs working.
//
// TCP-only by intent (the control-plane flows above set the precedent):
// the scenario needs prompt transport-death detection — TCP delivers
// EOF the moment the client drops, while rUDP has no FIN (its detach
// would ride the idle sweep, seconds of wall clock). The
// transport-generic mechanics underneath (Detach/Resume/RebindKey) are
// locked by gsb-core's reconnect suite, and the loadgen churn profile
// exercises the wire path at profile scale.

/// Drain `client`'s inbound frames for up to `window`, applying every
/// world snapshot to `view` (full snapshots replace it). Returns when
/// the window elapses or the connection dies (`Recv::Closed`).
async fn drain_snapshots(
    client: &mut Client,
    window: Duration,
    view: &mut std::collections::HashMap<u64, (i32, i32)>,
) {
    let deadline = Instant::now() + window;
    while let Some(remaining) = deadline.checked_duration_since(Instant::now()) {
        match client
            .recv(remaining.min(Duration::from_millis(100)))
            .await
            .unwrap()
        {
            Recv::Frame((op, payload)) if op == gsb_demo::op::WORLD_SNAPSHOT => {
                if let Ok(snap) = gsb_demo::game::WorldSnapshot::decode(&payload[..]) {
                    *view = snap
                        .entities
                        .iter()
                        .map(|e| (e.entity, (e.x, e.y)))
                        .collect();
                }
            }
            Recv::Frame(_) => {}
            Recv::Closed => return,
            Recv::TimedOut => {}
        }
    }
}

/// Wait until `view` shows `entity` at TWO different positions (the move
/// propagated through ingest → movement → snapshot → wire), sending
/// nothing else in the meantime.
async fn await_movement(
    client: &mut Client,
    entity: u64,
    view: &mut std::collections::HashMap<u64, (i32, i32)>,
    first: (i32, i32),
) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .unwrap_or_else(|| panic!("timed out waiting for entity {entity} to move"));
        match client.recv(remaining).await.unwrap() {
            Recv::Frame((op, payload)) if op == gsb_demo::op::WORLD_SNAPSHOT => {
                let snap = gsb_demo::game::WorldSnapshot::decode(&payload[..]).unwrap();
                *view = snap
                    .entities
                    .iter()
                    .map(|e| (e.entity, (e.x, e.y)))
                    .collect();
                if let Some(pos) = view.get(&entity)
                    && *pos != first
                {
                    return;
                }
            }
            Recv::Frame(_) => {}
            Recv::Closed => panic!("connection closed while waiting for movement"),
            Recv::TimedOut => {}
        }
    }
}

async fn resume_over_real_socket() {
    let mut cfg = cfg_on(Kind::Tcp, None, None, None);
    // The park grace must comfortably outlive the drop→reconnect gap so
    // the second session RESUMES instead of racing an expiry.
    cfg.disconnect_grace_secs = 8.0;
    let handle = gsb_server::start_server(cfg).await.expect("server starts");
    let addr = handle.addr;

    // -- observer takes a seat first ------------------------------------
    let mut obs = Client::connect(Kind::Tcp, addr)
        .await
        .expect("observer connects");
    let (op, payload) = auth_wire("obs", &[]);
    obs.write_frame(op, &payload).await.unwrap();
    let (op, payload) = join_wire(1);
    obs.write_frame(op, &payload).await.unwrap();
    // (auth result / join result consumed inline below via the shared
    //  helper shape: read until the join answer)
    let _deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let (op, payload) = match obs.recv(Duration::from_secs(5)).await.unwrap() {
            Recv::Frame(f) => f,
            _ => panic!("observer handshake stalled"),
        };
        if op == gsb_protocol::op::base::JOIN_ROOM_RESULT {
            let m = JoinRoomResult::decode(&payload[..]).unwrap();
            assert!(m.entity != 0);
            break;
        }
    }

    // -- hero joins ------------------------------------------------------
    let mut hero = Client::connect(Kind::Tcp, addr)
        .await
        .expect("hero connects");
    let (op, payload) = auth_wire("hero-rider", &[]);
    hero.write_frame(op, &payload).await.unwrap();
    let (op, payload) = join_wire(1);
    hero.write_frame(op, &payload).await.unwrap();
    // Uninitialized on purpose: the loop's only exits are the break
    // below (assignment happened) or a panic — a single write before use.
    let hero_entity: u64;
    loop {
        let (op, payload) = match hero.recv(Duration::from_secs(5)).await.unwrap() {
            Recv::Frame(f) => f,
            _ => panic!("hero handshake stalled"),
        };
        match op {
            gsb_protocol::op::base::AUTH_RESULT => {}
            gsb_protocol::op::base::JOIN_ROOM_RESULT => {
                let m = JoinRoomResult::decode(&payload[..]).unwrap();
                hero_entity = m.entity;
                break;
            }
            gsb_protocol::op::base::ERROR => {
                let m = Error::decode(&payload[..]).unwrap();
                panic!("hero join failed: code={} {}", m.code, m.message);
            }
            _ => {}
        }
    }

    // -- the hero moves once (input works pre-drop) ----------------------
    let move_wire = |x: i32, y: i32, seq: u64| gsb_demo::game::MoveTo { x, y, seq }.encode_to_vec();
    let payload = move_wire(-20, -20, 0);
    hero.write_frame(gsb_demo::op::MOVE_TO, &payload)
        .await
        .unwrap();

    let mut obs_view: std::collections::HashMap<u64, (i32, i32)> = std::collections::HashMap::new();
    // First: learn the hero's CURRENT (pre-command) position…
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut first_pos = None;
    while first_pos.is_none() {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .expect("no snapshot naming the hero");
        match obs.recv(remaining).await.unwrap() {
            Recv::Frame((op, payload)) if op == gsb_demo::op::WORLD_SNAPSHOT => {
                let snap = gsb_demo::game::WorldSnapshot::decode(&payload[..]).unwrap();
                obs_view = snap
                    .entities
                    .iter()
                    .map(|e| (e.entity, (e.x, e.y)))
                    .collect();
                first_pos = obs_view.get(&hero_entity).copied();
            }
            Recv::Frame(_) => {}
            _ => panic!("observer stalled"),
        }
    }
    // …then: the MOVE_TO must show up as a position change.
    await_movement(&mut obs, hero_entity, &mut obs_view, first_pos.unwrap()).await;

    // -- DROP without any LEAVE_ROOM_REQ ---------------------------------
    drop(hero);

    // The close cascade (reader-pump EOF → ConnClosed → Detach) settles.
    tokio::time::sleep(Duration::from_millis(700)).await;

    // Park proof: the OBSERVER still sees the hero in fresh snapshots —
    // the entity lives on while its socket is gone.
    obs_view.clear();
    drain_snapshots(&mut obs, Duration::from_millis(2500), &mut obs_view).await;
    assert!(
        obs_view.contains_key(&hero_entity),
        "parked hero vanished from the world while its socket was gone"
    );

    // -- RECONNECT with the SAME identity --------------------------------
    let mut hero2 = Client::connect(Kind::Tcp, addr).await.expect("reconnect");
    let (op, payload) = auth_wire("hero-rider", &[]);
    hero2.write_frame(op, &payload).await.unwrap();
    let (op, payload) = join_wire(1);
    hero2.write_frame(op, &payload).await.unwrap();
    loop {
        let (op, payload) = match hero2.recv(Duration::from_secs(5)).await.unwrap() {
            Recv::Frame(f) => f,
            _ => panic!("resume handshake stalled"),
        };
        match op {
            gsb_protocol::op::base::JOIN_ROOM_RESULT => {
                let m = JoinRoomResult::decode(&payload[..]).unwrap();
                assert_eq!(
                    m.entity, hero_entity,
                    "THE continuity contract: same wire id across sessions"
                );
                break;
            }
            gsb_protocol::op::base::ERROR => {
                let m = Error::decode(&payload[..]).unwrap();
                panic!("resume failed: code={} {}", m.code, m.message);
            }
            _ => {}
        }
    }

    // -- …and the new session's inputs WORK -------------------------------
    let payload = move_wire(30, 30, 1);
    hero2
        .write_frame(gsb_demo::op::MOVE_TO, &payload)
        .await
        .unwrap();
    // From wherever the park left it, the hero now converges toward
    // (30, 30): watch the OBSERVER's view for the approach.
    let deadline = Instant::now() + Duration::from_secs(15);
    let mut best = f32::MAX;
    loop {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .unwrap_or_else(|| panic!("resumed hero never moved toward (30,30): best {best}"));
        match obs.recv(remaining).await.unwrap() {
            Recv::Frame((op, payload)) if op == gsb_demo::op::WORLD_SNAPSHOT => {
                let snap = gsb_demo::game::WorldSnapshot::decode(&payload[..]).unwrap();
                obs_view = snap
                    .entities
                    .iter()
                    .map(|e| (e.entity, (e.x, e.y)))
                    .collect();
                if let Some(&(x, y)) = obs_view.get(&hero_entity) {
                    let (dx, dy) = ((x - 30) as f32, (y - 30) as f32);
                    best = best.min((dx * dx + dy * dy).sqrt());
                    if best < 8.0 {
                        break;
                    }
                }
            }
            Recv::Frame(_) => {}
            Recv::Closed => panic!("observer closed"),
            Recv::TimedOut => {}
        }
    }

    handle.stop().await;
}

#[tokio::test]
async fn dropped_socket_parks_and_same_identity_resumes_with_the_same_entity() {
    resume_over_real_socket().await;
}
