//! Tur B pre-auth guardrails (docs/SECURITY.md §3 + §4):
//!
//! - the AUTH attempt window (§3.1): three attempts per ten seconds per
//!   connection; the fourth-plus in-window rides the EXISTING violation
//!   budget as a hard violation — attempts one-to-three keep the normal
//!   ticket-rejection path (ERROR 10, connection alive);
//! - the heartbeat-ACK throttle (§3.2): at most one ACK per second, in
//!   BOTH connection phases; surplus heartbeats are counted in a
//!   dedicated counter per phase and deliberately NOT budgeted (a
//!   buggy-but-honest client must not burn its budget on liveness probes
//!   — the throttle itself already caps the amplification). The clock
//!   spans the auth boundary with ONE reset at auth success;
//! - the pre-auth frame budget (§3.3): 64 inbound frames of any kind
//!   before auth success, crossing = immediate close (ERROR 9 naming the
//!   policy); auth success retires the counter naturally;
//! - the unauthenticated-session cap (§4): the registry tracks conn auth
//!   state (`RegistryMsg::Authed` after success); over-cap new conns get
//!   the gentle birth rejection; detached/resumed sessions never count
//!   (they carry tickets, so they are authenticated by construction).
//!
//! Decision documented here (the contract allowed either reading): the
//! FOURTH in-window AUTH attempt is ANSWERED with the rate-limit
//! diagnosis (ERROR code 3, the auth family — this connection's violation
//! count is still under the answer limit) and scored against the budget;
//! it is never answered by the ticket-rejection path. Only after three
//! such answers does the funnel go silent, and four of them close — the
//! ordinary budget progression, reused unchanged.
//!
//! Connection-level tests drive the actor directly over its inbox (the
//! `violation.rs` idiom); registry-level tests drive the control plane
//! over `RegistryMsg` (the `reconnect.rs` idiom, with a hold-on-disconnect
//! logic so a parked/resumed session exists to be counted — or not).

use std::collections::HashMap;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use gsb_core::auth::{TicketAuth, TicketError, TicketValidator, ValidatedTicket};
use gsb_core::channel::{FrameBatch, channel};
use gsb_core::conn::{ConnIn, ConnectionActor};
use gsb_core::id::{ConnectionId, EntityId, PlayerId, RoomId};
use gsb_core::metrics::MetricsEvent;
use gsb_core::registry::{BuiltRoom, Registry, RegistryMsg, RoomFactory};
use gsb_core::room::{
    Action, Admission, Detach, ExpireTo, GameLogic, ResumeFound, RoomConfig, RoomLogic, TickCtx,
};
use gsb_core::shard::BorderRecord;
use gsb_core::ticker::Ticker;
use gsb_protocol::base::{Auth, Heartbeat};
use gsb_protocol::{base_table, op};
use prost::Message;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

const WAIT: Duration = Duration::from_secs(5);

// =====================================================================
// Connection-level harness
// =====================================================================

fn frame(op: u16, payload: &[u8]) -> gsb_protocol::FrameBody {
    gsb_protocol::FrameBody::new(op, payload.to_vec())
}

fn auth_frame(name: &str, ticket: &[u8]) -> gsb_protocol::FrameBody {
    let a = Auth {
        name: name.into(),
        ticket: ticket.to_vec(),
        protocol_version: 0,
    };
    frame(op::base::AUTH_REQ, &a.encode_to_vec())
}

/// A connection actor with the given ticket hook (`None` = local auth)
/// and a DEAD registry (every registry send fails — the shutdown-shaped
/// condition; every §3 mechanism here is actor-local, so the registry is
/// irrelevant except in `authed_notice_reaches_the_registry`'s live twin
/// below).
fn spawn_actor(
    conn: u64,
    auth: Option<TicketAuth>,
) -> (
    mpsc::Sender<ConnIn>,
    mpsc::Receiver<FrameBatch>,
    JoinHandle<()>,
) {
    let (inbox_tx, inbox) = channel::<ConnIn>(128);
    let (out_tx, out_rx) = channel::<FrameBatch>(16);
    let (reg_tx, _reg_rx) = channel::<RegistryMsg>(16);
    let (metrics_tx, _metrics_rx) = mpsc::channel::<MetricsEvent>(16);
    let actor = ConnectionActor::new(
        ConnectionId(conn),
        SocketAddr::from(([127, 0, 0, 1], 42_000u16 + conn as u16)),
        Arc::new(base_table()),
        reg_tx,
        inbox,
        out_tx,
        metrics_tx,
        auth,
    );
    let h = tokio::spawn(actor.run());
    (inbox_tx, out_rx, h)
}

/// Local-auth actor with a LIVE registry mailbox, so the §4 `Authed`
/// notice can be observed crossing the seam.
fn spawn_actor_live_registry(
    conn: u64,
) -> (
    mpsc::Sender<ConnIn>,
    mpsc::Receiver<FrameBatch>,
    gsb_core::channel::Inbox<RegistryMsg>,
    JoinHandle<()>,
) {
    let (inbox_tx, inbox) = channel::<ConnIn>(128);
    let (out_tx, out_rx) = channel::<FrameBatch>(16);
    let (reg_tx, reg_rx) = channel::<RegistryMsg>(16);
    let (metrics_tx, _metrics_rx) = mpsc::channel::<MetricsEvent>(16);
    let actor = ConnectionActor::new(
        ConnectionId(conn),
        SocketAddr::from(([127, 0, 0, 1], 43_000u16 + conn as u16)),
        Arc::new(base_table()),
        reg_tx,
        inbox,
        out_tx,
        metrics_tx,
        None,
    );
    let h = tokio::spawn(actor.run());
    (inbox_tx, out_rx, reg_rx, h)
}

/// A validator that rejects EVERY ticket (normal rejection, ERROR 10):
/// the credential-stuffing shape the §3.1 window exists for.
fn rejecting_validator() -> TicketValidator {
    Arc::new(|_: bytes::Bytes| {
        Box::pin(async { Err(TicketError::Rejected("bad ticket".into())) })
            as Pin<Box<dyn Future<Output = Result<ValidatedTicket, TicketError>> + Send>>
    })
}

fn rejecting_hook() -> TicketAuth {
    TicketAuth {
        validator: rejecting_validator(),
        timeout: Duration::from_secs(2),
    }
}

async fn next_batch(out: &mut mpsc::Receiver<FrameBatch>) -> FrameBatch {
    tokio::time::timeout(WAIT, out.recv())
        .await
        .expect("timed out")
        .expect("out channel closed")
}

fn error_in(batch: &FrameBatch) -> Option<(i32, String)> {
    batch.iter().find(|f| f.op == op::base::ERROR).map(|f| {
        let e = gsb_protocol::base::Error::decode(f.payload.as_ref()).expect("Error decode");
        (e.code, e.message)
    })
}

fn has_op(batch: &FrameBatch, want: u16) -> bool {
    batch.iter().any(|f| f.op == want)
}

/// Short-window silence check: nothing arrives within 400 ms.
async fn expect_quiet(out: &mut mpsc::Receiver<FrameBatch>) {
    match tokio::time::timeout(Duration::from_millis(400), out.recv()).await {
        Err(_) => {} // silence, as expected
        Ok(None) => panic!("out channel closed unexpectedly"),
        Ok(Some(b)) => panic!("expected silence, got batch: {b:?}"),
    }
}

// =====================================================================
// 1 — §3.1: the flood case (budget exhaustion → close)
// =====================================================================

#[tokio::test]
async fn auth_flood_exhausts_violation_budget_and_closes() {
    let (in_tx, mut out, handle) = spawn_actor(1, Some(rejecting_hook()));
    // Seven rapid AUTH attempts against a server that rejects every
    // ticket. Attempts 1–3 are admitted by the window and answered by the
    // NORMAL ticket-rejection path (ERROR 10, alive). Attempts 4–7 are
    // past the allowance: HARD violations (weight 4), riding the budget —
    // answered on 4–6 (under the answer limit), silent on 7 where the
    // score reaches 16 and the connection closes.
    for _ in 0..7 {
        in_tx
            .send(ConnIn::Frame(auth_frame("bot", b"stolen")))
            .await
            .expect("inbox open");
    }
    let mut codes = Vec::new();
    for _ in 0..7 {
        let batch = next_batch(&mut out).await;
        let (code, msg) =
            error_in(&batch).unwrap_or_else(|| panic!("batch without ERROR: {batch:?}"));
        codes.push(code);
        if code == 9 {
            assert!(
                msg.contains("violation"),
                "the close must name the violation budget: {msg}"
            );
        } else if code != 10 && code != 3 {
            panic!("unexpected error code {code}: {msg}");
        }
    }
    assert_eq!(
        codes,
        vec![10, 10, 10, 3, 3, 3, 9],
        "three ticket rejections, three rate-limit diagnoses, then the close"
    );
    // Teardown: the actor dropped its out sender.
    assert!(
        tokio::time::timeout(WAIT, out.recv())
            .await
            .unwrap()
            .is_none(),
        "no frames after the budget close"
    );
    handle.await.expect("actor exits");
}

// =====================================================================
// 2 — §3.1: the honest-retry boundary (attempts 1–3 vs the fourth)
// =====================================================================

#[tokio::test]
async fn three_auth_attempts_in_window_answered_fourth_counts() {
    let (in_tx, mut out, handle) = spawn_actor(2, Some(rejecting_hook()));
    // Attempts 1–3 inside the window: the NORMAL ticket-rejection answer
    // (ERROR 10) every time — a legitimate client retrying a rejected
    // ticket is never budgeted for trying.
    for _ in 0..3 {
        in_tx
            .send(ConnIn::Frame(auth_frame("ana", b"expired")))
            .await
            .expect("inbox open");
        let batch = next_batch(&mut out).await;
        let (code, msg) =
            error_in(&batch).unwrap_or_else(|| panic!("batch without ERROR: {batch:?}"));
        assert_eq!(code, 10, "attempt 1-3 must take the ticket path: {msg}");
    }
    // The FOURTH attempt in-window: NOT the ticket-rejection path. It is
    // budget-attributed — answered here with the rate-limit diagnosis
    // (code 3, auth family; this is the connection's FIRST violation, so
    // the funnel is still in its answering phase) and scored (weight 4).
    in_tx
        .send(ConnIn::Frame(auth_frame("ana", b"expired")))
        .await
        .expect("inbox open");
    let batch = next_batch(&mut out).await;
    let (code, msg) = error_in(&batch).expect("the fourth attempt is answered");
    assert_ne!(code, 10, "the fourth attempt must NOT take the ticket path");
    assert_eq!(code, 3, "rate-limit diagnosis uses the auth-family code");
    assert!(
        msg.contains("rate limit"),
        "the diagnosis must name the limit: {msg}"
    );
    // Score is now 4 < 16: the connection is ALIVE (a fifth in-window
    // attempt is diagnosed the same way — the window persists), and the
    // client can still recover by waiting out the ten seconds.
    in_tx
        .send(ConnIn::Frame(auth_frame("ana", b"expired")))
        .await
        .expect("inbox open");
    let batch = next_batch(&mut out).await;
    let (code, _) = error_in(&batch).expect("fifth attempt answered");
    assert_eq!(code, 3);
    // Clean shutdown: proves the run loop was never torn down.
    in_tx.send(ConnIn::Shutdown).await.expect("inbox open");
    handle.await.expect("actor exits cleanly on Shutdown");
}

// =====================================================================
// 3 — §3.2: pre-auth heartbeat throttle (+ post-auth untouched),
//     plus the §4 Authed notice observed at the seam
// =====================================================================

#[tokio::test]
async fn preauth_heartbeat_flood_is_counted_not_answered() {
    let (in_tx, mut out, mut reg_rx, handle) = spawn_actor_live_registry(3);
    let hb = Heartbeat { tick: 1 }.encode_to_vec();
    // Five pre-auth heartbeats back-to-back: ONE ack (the first), the
    // surplus counted silently — no ERROR frames at all (they are not
    // violations; a violating flood would have been answered/closed).
    for _ in 0..5 {
        in_tx
            .send(ConnIn::Frame(frame(op::base::HEARTBEAT, &hb)))
            .await
            .expect("inbox open");
    }
    let batch = next_batch(&mut out).await;
    assert!(
        has_op(&batch, op::base::HEARTBEAT_ACK),
        "the first pre-auth heartbeat is answered"
    );
    expect_quiet(&mut out).await;
    // Authenticate (local path): the §4 notice crosses to the registry…
    let auth = Auth {
        name: "ana".into(),
        ticket: vec![],
        protocol_version: 0,
    }
    .encode_to_vec();
    in_tx
        .send(ConnIn::Frame(frame(op::base::AUTH_REQ, &auth)))
        .await
        .expect("inbox open");
    let batch = next_batch(&mut out).await;
    assert!(has_op(&batch, op::base::AUTH_RESULT), "auth succeeds");
    match tokio::time::timeout(WAIT, reg_rx.recv())
        .await
        .expect("timed out")
    {
        Some(RegistryMsg::Authed { conn }) => assert_eq!(conn, ConnectionId(3)),
        other => panic!("expected RegistryMsg::Authed, got {other:?}"),
    }
    // …and auth success restarts the throttle's clock exactly once: the
    // FIRST post-auth heartbeat is answered even though a pre-auth ACK
    // went out moments ago (a client probing right after AUTH must never
    // read silence as a dead server)…
    in_tx
        .send(ConnIn::Frame(frame(op::base::HEARTBEAT, &hb)))
        .await
        .expect("inbox open");
    let batch = next_batch(&mut out).await;
    assert!(
        has_op(&batch, op::base::HEARTBEAT_ACK),
        "the first post-auth heartbeat is answered"
    );
    // …after which the SAME throttle keeps running: a burst buys one
    // answer per interval and the rest are counted, not answered — and
    // still not violations (no ERROR frame anywhere).
    for _ in 0..3 {
        in_tx
            .send(ConnIn::Frame(frame(op::base::HEARTBEAT, &hb)))
            .await
            .expect("inbox open");
    }
    expect_quiet(&mut out).await;
    in_tx.send(ConnIn::Shutdown).await.expect("inbox open");
    handle.await.expect("actor exits cleanly on Shutdown");
}

/// THE POST-AUTH HALF of §3.2: the same one-answer-per-interval throttle
/// keeps running after auth success. A well-behaved client heartbeats at
/// roughly the throttle's own cadence, so nothing about its RTT semantics
/// changes; a client sending a burst gets ONE answer per interval and the
/// rest are counted silently.
///
/// Three properties at once, and the third is the point: the surplus is
/// NOT charged to the violation budget. A chatty NAT keepalive or a
/// client with a misconfigured heartbeat timer is not hostile the way an
/// undefined opcode is, and the throttle already caps what the flood can
/// buy (one small ACK per interval per connection), so scoring it would
/// only disconnect honest-but-buggy clients. That reasoning is the
/// pre-auth field doc's, kept consistent across the auth boundary.
#[tokio::test]
async fn postauth_heartbeat_flood_is_answered_once_counted_and_never_scored() {
    let (in_tx, mut out, handle) = spawn_actor(9, None);
    // Authenticate first: this test is entirely about the post-auth side.
    in_tx
        .send(ConnIn::Frame(auth_frame("ana", b"")))
        .await
        .expect("inbox open");
    let batch = next_batch(&mut out).await;
    assert!(has_op(&batch, op::base::AUTH_RESULT), "auth succeeds");

    // A burst far past the violation budget (16 / weight 4 = four hard
    // violations close a connection): if any of these were scored, the
    // session would be gone long before the burst ended.
    let hb = Heartbeat { tick: 1 }.encode_to_vec();
    for _ in 0..40 {
        in_tx
            .send(ConnIn::Frame(frame(op::base::HEARTBEAT, &hb)))
            .await
            .expect("inbox open");
    }
    let batch = next_batch(&mut out).await;
    assert!(
        has_op(&batch, op::base::HEARTBEAT_ACK),
        "the first post-auth heartbeat is answered (a client that keeps \
         the normal cadence sees no change at all)"
    );
    // …and the other thirty-nine buy exactly nothing: no ACK, and no
    // ERROR either — they are counted, not violations.
    expect_quiet(&mut out).await;

    // The session is alive and unscored: after the interval the next
    // heartbeat is answered again, on the same connection.
    tokio::time::sleep(Duration::from_millis(1100)).await;
    let hb = Heartbeat { tick: 2 }.encode_to_vec();
    in_tx
        .send(ConnIn::Frame(frame(op::base::HEARTBEAT, &hb)))
        .await
        .expect("inbox open");
    let batch = next_batch(&mut out).await;
    assert!(
        has_op(&batch, op::base::HEARTBEAT_ACK),
        "the throttle answers again once the interval has passed: {batch:?}"
    );
    let ack = batch
        .iter()
        .find(|f| f.op == op::base::HEARTBEAT_ACK)
        .expect("the ack is in this batch");
    assert_eq!(
        gsb_protocol::base::HeartbeatAck::decode(ack.payload.as_ref())
            .expect("HeartbeatAck decode")
            .tick,
        2,
        "the answer still echoes the heartbeat's tick: the RTT semantics \
         a client reads off the ACK are untouched"
    );

    // Clean shutdown proves the run loop was never torn down.
    in_tx.send(ConnIn::Shutdown).await.expect("inbox open");
    handle.await.expect("actor exits cleanly on Shutdown");
}

// =====================================================================
// 4 — §3.3: the pre-auth total frame budget
// =====================================================================

#[tokio::test]
async fn preauth_frame_budget_closes_at_64() {
    let (in_tx, mut out, handle) = spawn_actor(4, None);
    // Heartbeats are the perfect probe: well-formed (no violation), but
    // frames 2..=64 fall under the §3.2 throttle and stay silent, isolating
    // the §3.3 close. Frame 65 crosses the budget: immediate close, the
    // frame itself never processed.
    let hb = Heartbeat { tick: 7 }.encode_to_vec();
    for i in 0..65 {
        in_tx
            .send(ConnIn::Frame(frame(op::base::HEARTBEAT, &hb)))
            .await
            .expect("inbox open");
        if i == 0 {
            let batch = next_batch(&mut out).await;
            assert!(has_op(&batch, op::base::HEARTBEAT_ACK));
        }
    }
    let batch = next_batch(&mut out).await;
    let (code, msg) = error_in(&batch).unwrap_or_else(|| panic!("batch without ERROR: {batch:?}"));
    assert_eq!(code, 9, "a policy close uses the server-decision code");
    assert!(
        msg.contains("pre-auth frame budget"),
        "the reason must name the policy: {msg}"
    );
    assert!(
        tokio::time::timeout(WAIT, out.recv())
            .await
            .unwrap()
            .is_none(),
        "teardown after the pre-auth budget close"
    );
    handle.await.expect("actor exits");
}

// =====================================================================
// Registry-level harness (§4): a hold-everything logic so parked/resumed
// sessions exist (reconnect.rs idiom).
// =====================================================================

const SNAP_OP: u16 = 0x7600;

#[derive(Debug, Clone, Copy)]
enum Ledg {
    Held(PlayerId),
}

/// Holds every disconnect for an hour and serves resumes from a tiny
/// ledger: enough room-side machinery for a parked identity to exist and
/// be resumed onto its SAME wire id.
struct HoldLogic {
    next: u64,
    player_entity: HashMap<PlayerId, EntityId>,
    ledger: HashMap<String, Ledg>,
}

impl GameLogic<()> for HoldLogic {
    type GroupKey = ();
    type Strip = ();

    fn snapshot_op(&self) -> u16 {
        SNAP_OP
    }
    fn private_op(&self) -> u16 {
        SNAP_OP + 1
    }
    fn group_of(&self, _w: &(), _p: PlayerId) -> Self::GroupKey {}

    fn snapshot(
        &mut self,
        _w: &mut (),
        _c: &TickCtx,
        _g: &Self::GroupKey,
        _borrowed: &[BorderRecord<()>],
        out: &mut bytes::BytesMut,
    ) -> bool {
        let mut ids: Vec<EntityId> = self.player_entity.values().copied().collect();
        ids.sort_unstable();
        for id in ids {
            out.extend_from_slice(&id.to_le_bytes());
        }
        true
    }

    fn on_join(&mut self, _w: &mut (), conn: ConnectionId) -> Admission {
        self.next += 1;
        let player = PlayerId(conn.0);
        self.player_entity.insert(player, self.next);
        Admission {
            player,
            entity: self.next,
        }
    }

    fn on_leave(&mut self, _w: &mut (), player: PlayerId) {
        self.player_entity.remove(&player);
    }

    fn on_disconnect(&mut self, _w: &mut (), player: PlayerId, identity: &str) -> Detach {
        if self.player_entity.contains_key(&player) {
            self.ledger.insert(identity.to_string(), Ledg::Held(player));
        }
        Detach::Hold {
            grace: Some(Duration::from_secs(3600)),
            to: ExpireTo::Despawn,
        }
    }

    fn resume_lookup(&self, _w: &(), identity: &str) -> ResumeFound {
        match self.ledger.get(identity) {
            Some(Ledg::Held(p)) => ResumeFound::Held(*p),
            None => ResumeFound::Never,
        }
    }

    fn on_resume(
        &mut self,
        _w: &mut (),
        identity: &str,
        _c: ConnectionId,
        _p: PlayerId,
        _e: EntityId,
    ) {
        self.ledger.remove(identity);
    }

    fn ingest(&mut self, _w: &mut (), _c: &TickCtx, actions: &mut Vec<Action>) {
        actions.clear();
    }

    fn update(&mut self, _w: &mut (), _c: &TickCtx) {}
}

impl RoomLogic<()> for HoldLogic {}

fn hold_factory() -> RoomFactory<(), (), (), ()> {
    Arc::new(|_id, _cfg| BuiltRoom::Single {
        world: (),
        logic: Box::new(HoldLogic {
            next: 0,
            player_entity: HashMap::new(),
            ledger: HashMap::new(),
        }),
    })
}

/// A registry with ONLY the unauthenticated cap set (total cap unlimited):
/// the §4 guardrail must stand on its own.
fn start_registry(max_unauth_conns: u64) -> (mpsc::Sender<RegistryMsg>, JoinHandle<()>) {
    let (tx, rx) = channel::<RegistryMsg>(4096);
    let (ticker, _ticker_task) = Ticker::spawn(60.0, 64).expect("valid tick rate");
    let (metrics_tx, _metrics_rx) = mpsc::channel::<MetricsEvent>(64);
    let handle = tokio::spawn(
        Registry::new(
            rx,
            tx.clone(),
            hold_factory(),
            ticker,
            metrics_tx,
            None,
            Some(max_unauth_conns),
            None,
        )
        .run(),
    );
    (tx, handle)
}

async fn create_room(tx: &mpsc::Sender<RegistryMsg>, id: RoomId) {
    let (reply_tx, reply_rx) = oneshot::channel();
    tx.send(RegistryMsg::CreateRoom {
        config: RoomConfig {
            id,
            tick_hz: 60.0,
            ..Default::default()
        },
        reply: reply_tx,
    })
    .await
    .expect("registry gone");
    tokio::time::timeout(WAIT, reply_rx)
        .await
        .expect("timed out")
        .expect("reply dropped")
        .expect("room creation failed");
}

async fn open_conn(tx: &mpsc::Sender<RegistryMsg>, conn: ConnectionId) -> mpsc::Receiver<ConnIn> {
    let (inbox_tx, inbox_rx) = mpsc::channel(16);
    tx.send(RegistryMsg::ConnOpened {
        conn,
        inbox: inbox_tx,
    })
    .await
    .expect("registry gone");
    inbox_rx
}

async fn mark_authed(tx: &mpsc::Sender<RegistryMsg>, conn: ConnectionId) {
    tx.send(RegistryMsg::Authed { conn })
        .await
        .expect("registry gone");
}

async fn spawn_as(
    tx: &mpsc::Sender<RegistryMsg>,
    conn: ConnectionId,
    room: RoomId,
    identity: &str,
) -> Result<EntityId, gsb_core::error::CoreError> {
    let (reply_tx, reply_rx) = oneshot::channel();
    let (out_tx, _out_rx) = mpsc::channel(64);
    tx.send(RegistryMsg::SpawnPlayer {
        conn,
        room,
        out: out_tx,
        identity: identity.to_string(),
        reply: reply_tx,
    })
    .await
    .expect("registry gone");
    match tokio::time::timeout(WAIT, reply_rx).await {
        Ok(Ok(result)) => result.map(|(e, _)| e),
        other => panic!("spawn round trip failed: {other:?}"),
    }
}

async fn expect_birth_rejection(mut inbox: mpsc::Receiver<ConnIn>, why: &str) {
    match tokio::time::timeout(WAIT, inbox.recv())
        .await
        .expect("timed out")
    {
        Some(ConnIn::ServerClosed { cause, reason }) => {
            assert_eq!(
                cause,
                gsb_core::conn::ServerClose::UnauthCap,
                "{why}: the refusal must carry its counted reason"
            );
            assert!(
                reason.contains("unauthenticated capacity"),
                "{why}: wrong rejection reason: {reason}"
            );
        }
        other => panic!("{why}: expected ServerClosed, got {other:?}"),
    }
}

async fn expect_accepted(mut inbox: mpsc::Receiver<ConnIn>, why: &str) {
    match tokio::time::timeout(Duration::from_millis(400), inbox.recv()).await {
        Err(_) => {} // nothing arrived: the connection was recorded, not rejected
        Ok(None) => panic!("{why}: inbox closed unexpectedly"),
        Ok(Some(msg)) => panic!("{why}: unexpected message {msg:?}"),
    }
}

async fn stop_registry(tx: mpsc::Sender<RegistryMsg>, handle: JoinHandle<()>) {
    tx.send(RegistryMsg::Shutdown).await.ok();
    drop(tx);
    handle.await.ok();
}

// =====================================================================
// 5 — §4: the cap rejects new conns; authed conns survive
// =====================================================================

#[tokio::test]
async fn unauthed_cap_rejects_new_conns_while_authed_survive() {
    let (tx, handle) = start_registry(1);

    // c1 takes the single unauthenticated seat.
    let _c1 = open_conn(&tx, ConnectionId(1)).await;
    // c2 arrives while the seat is held: gentle birth rejection (ERROR 9
    // via ServerClosed), no table entry.
    let c2 = open_conn(&tx, ConnectionId(2)).await;
    expect_birth_rejection(c2, "second conn over the unauth cap").await;

    // c1 authenticates: it leaves the pool (this is the exact transition
    // the connection actor reports after a successful AUTH).
    mark_authed(&tx, ConnectionId(1)).await;

    // Now c3 fits — the authed c1 is unaffected throughout (its entry, and
    // its ability to proceed to a join, were never touched).
    let c3 = open_conn(&tx, ConnectionId(3)).await;
    expect_accepted(c3, "c3 must fit once c1 authed").await;
    // And the seat is genuinely occupied again: c4 is rejected, proving
    // c3 was RECORDED (not silently dropped) by the cap's accounting.
    let c4 = open_conn(&tx, ConnectionId(4)).await;
    expect_birth_rejection(c4, "fourth conn over the refilled cap").await;

    stop_registry(tx, handle).await;
}

// =====================================================================
// 6 — §4: detached/resumed sessions never consume unauth capacity
// =====================================================================

#[tokio::test]
async fn resume_counts_as_authed() {
    let (tx, handle) = start_registry(2);
    create_room(&tx, RoomId(9)).await;

    // Session 1 ("ana"): open (transient seat) → auth (seat released) →
    // join → transport dies → PARKED. The parked entry keeps its room
    // affiliation AND its authenticated mark: it must never re-enter the
    // unauthenticated pool.
    let _c1 = open_conn(&tx, ConnectionId(1)).await;
    mark_authed(&tx, ConnectionId(1)).await;
    let e1 = spawn_as(&tx, ConnectionId(1), RoomId(9), "ana")
        .await
        .expect("first session joins");
    tx.send(RegistryMsg::ConnClosed {
        conn: ConnectionId(1),
    })
    .await
    .expect("registry gone");
    // Let the dispatcher's detach settle (the reconnect.rs idiom).
    tokio::time::sleep(Duration::from_millis(200)).await;

    // Two fresh unauthenticated connections now BOTH fit (cap 2): proof
    // that the parked ana consumes zero unauth capacity — under a broken
    // accounting the second of these would already have been rejected.
    let c2 = open_conn(&tx, ConnectionId(2)).await;
    expect_accepted(c2, "parked session must not hold an unauth seat (1/2)").await;
    let c3 = open_conn(&tx, ConnectionId(3)).await;
    expect_accepted(c3, "parked session must not hold an unauth seat (2/2)").await;
    // Pool full: a third fresh conn is rejected.
    let c4 = open_conn(&tx, ConnectionId(4)).await;
    expect_birth_rejection(c4, "cap enforced against fresh conns alone").await;

    // c2 finishes its handshake the normal way (the actor would report
    // exactly this): a seat frees up for the resuming session to arrive.
    mark_authed(&tx, ConnectionId(2)).await;

    // The RESUME: ana's new session opens into the freed seat, leaves the
    // pool again the moment it authenticates, and rejoins onto the SAME
    // wire id through the park ledger — the full resume path works
    // untouched while other connections hold the remaining seat.
    let _c5 = open_conn(&tx, ConnectionId(5)).await;
    mark_authed(&tx, ConnectionId(5)).await;
    let e5 = spawn_as(&tx, ConnectionId(5), RoomId(9), "ana")
        .await
        .expect("resume accepted under the cap");
    assert_eq!(e5, e1, "the resume carries the same wire id");

    // And the freshly-authed resume freed its transient seat at once: a
    // new connection fits again (only c3's seat is still held).
    let c6 = open_conn(&tx, ConnectionId(6)).await;
    expect_accepted(c6, "the authed resume released its seat").await;

    stop_registry(tx, handle).await;
}
