//! The correlated-request (RPC) pattern invariants (feature B):
//!
//! - a room-local request is answered in the SAME tick, on the
//!   per-connection private frame, to the requesting connection ONLY
//!   (no leak to any other connection);
//! - an external-I/O request does NOT block the tick (the room keeps
//!   stepping while it is in flight) and its answer arrives on a LATER
//!   tick, on the same private path, to the requesting connection;
//! - a connection that leaves while a request is in flight frees its
//!   pending slots (the caps are not leaked) and its late worker reports
//!   are dropped (no double answer, no delivery to a gone session);
//! - the room's request timeout is the client-visible authority: a
//!   stuck future is answered with a timeout reply, and the worker's
//!   late report is dropped (exactly one answer per request);
//! - the id-space rules (duplicate in-flight id rejected; answered id
//!   reusable), the pending caps (per connection + room-wide), and the
//!   malformed/envelope edge cases are all normal rejections (answered
//!   in the same tick, never left hanging).
//!
//! The tests drive a `RoomActor` directly (the room is the actor that
//! owns the mechanism — see `gsb_core::rpc` for where the pending state
//! lives and why). The test logic reports its external requests'
//! resolvers through a channel so the tests control when (or whether)
//! a delegated request resolves.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use bytes::BufMut;
use gsb_core::channel::{FrameBatch, Mailbox, channel};
use gsb_core::id::{ConnectionId, PlayerId, RoomId};
use gsb_core::metrics::{MetricsEvent, RoomSample};
use gsb_core::room::{
    Action, Admission, GameLogic, RoomActor, RoomConfig, RoomControl, RoomLogic, TickCtx,
};
use gsb_core::rpc::RequestDecision;
use gsb_core::ticker::TickInfo;
use gsb_protocol::base;
use prost::Message;
use tokio::sync::{broadcast, mpsc, oneshot};

// Test opcodes (inner game-band ops; the room does not validate them —
// the logic decides).
const OP_LOCAL: u16 = 0x01;
const OP_REJECT: u16 = 0x02;
const OP_EXT: u16 = 0x03;
const OP_EXT_IMMEDIATE: u16 = 0x04;
const OP_UNKNOWN: u16 = 0x05;

// Child modules of this test binary (an integration-test root needs an
// explicit path: `tests/<name>.rs` is the crate root, so a bare `mod`
// would look for `tests/buckets.rs`).
#[path = "rpc/buckets.rs"]
mod buckets;
#[path = "rpc/counters.rs"]
mod counters;
#[path = "rpc/paused.rs"]
mod paused;
#[path = "rpc/unread.rs"]
mod unread;

const OP_SNAP: u16 = 0x1001;
const OP_PRIV: u16 = 0x1002;

/// One in-flight external request the test logic has delegated: its
/// correlation id and the resolver (sending to it completes the request
/// with `Ok(payload)`; sending `Err(reason)` rejects it; dropping it
/// leaves the future pending until the room's timeout).
struct Resolver {
    id: u64,
    resolve: oneshot::Sender<Result<Vec<u8>, String>>,
}

/// The test logic:
///
/// - `OP_LOCAL` → room-local `Reply` (payload = the id, 8 LE bytes);
/// - `OP_REJECT` → room-local `Reject`;
/// - `OP_EXT` → `External` whose future completes when the test sends
///   through its resolver (pushed to the test over `ext_tx`);
/// - `OP_EXT_IMMEDIATE` → `External` that resolves to `Ok(b"done")` on
///   its first poll (no test involvement);
/// - anything else → `None` (the core's "no handler" rejection).
///
/// The private seam encodes each reply as `[id: u64 LE][ok: u8]` — a
/// test-local format (the core must not depend on the game's proto).
struct RpcLogic {
    ext_tx: mpsc::UnboundedSender<Resolver>,
}

impl GameLogic<()> for RpcLogic {
    type GroupKey = ();
    type Strip = ();

    fn snapshot_op(&self) -> u16 {
        OP_SNAP
    }
    fn private_op(&self) -> u16 {
        OP_PRIV
    }

    fn group_of(&self, _w: &(), _p: PlayerId) -> Self::GroupKey {}

    fn snapshot(
        &mut self,
        _w: &mut (),
        _c: &TickCtx,
        _g: &(),
        _borrowed: &[gsb_core::shard::BorderRecord<()>],
        _o: &mut bytes::BytesMut,
    ) -> bool {
        false // no group snapshots: batches carry only private frames
    }

    fn private(
        &mut self,
        _w: &mut (),
        _player: PlayerId,
        _g: &(),
        responses: &[gsb_core::rpc::RpcReply],
        out: &mut bytes::BytesMut,
    ) -> bool {
        if responses.is_empty() {
            return false;
        }
        for r in responses {
            out.extend_from_slice(&r.id.to_le_bytes());
            out.put_u8(if r.ok { 1 } else { 0 });
        }
        true
    }

    fn on_join(&mut self, _w: &mut (), c: ConnectionId) -> Admission {
        Admission {
            player: PlayerId(c.0),
            entity: 1,
        }
    }
    fn on_leave(&mut self, _w: &mut (), _p: PlayerId) {}
    fn ingest(&mut self, _w: &mut (), _c: &TickCtx, _a: &mut Vec<Action>) {}
    fn update(&mut self, _w: &mut (), _c: &TickCtx) {}

    // Faz 3 trait split: the request seam lives on the shared
    // `GameLogic` supertrait (the shard actor runs it too).
    fn handle_request(
        &mut self,
        _w: &mut (),
        _ctx: &TickCtx,
        req: &gsb_core::rpc::RpcRequest,
    ) -> Option<RequestDecision> {
        match req.op {
            OP_LOCAL => {
                let mut payload = Vec::with_capacity(8);
                payload.extend_from_slice(&req.id.to_le_bytes());
                Some(RequestDecision::Reply(payload.into()))
            }
            OP_REJECT => Some(RequestDecision::Reject("logic rejected".to_string())),
            OP_EXT => {
                // The resolver is handed to the test from INSIDE the
                // future (on first poll — i.e. only once the room has
                // committed the registration and spawned the worker).
                // A cap/duplicate-rejected External decision drops the
                // future before its first poll, so a rejected request
                // never produces a resolver (no orphan handles).
                let id = req.id;
                let ext_tx = self.ext_tx.clone();
                let fut = async move {
                    let (resolve, rx) = oneshot::channel::<Result<Vec<u8>, String>>();
                    let _ = ext_tx.send(Resolver { id, resolve });
                    match rx.await {
                        Ok(Ok(payload)) => Ok(payload.into()),
                        Ok(Err(reason)) => Err(reason),
                        Err(_) => Err("resolver dropped".to_string()),
                    }
                };
                Some(RequestDecision::External(Box::pin(fut)))
            }
            OP_EXT_IMMEDIATE => {
                let fut = async { Ok(bytes::Bytes::from_static(b"done")) };
                Some(RequestDecision::External(Box::pin(fut)))
            }
            _ => None,
        }
    }
}

// Faz 3 promotion: `handle_request` moved to `GameLogic`; this impl stays
// as the single-room marker.
impl RoomLogic<()> for RpcLogic {}

struct Harness {
    tick_tx: broadcast::Sender<TickInfo>,
    control: Mailbox<RoomControl>,
    handle: tokio::task::JoinHandle<()>,
    t0: Instant,
    next_tick: u64,
    resolvers: mpsc::UnboundedReceiver<Resolver>,
    /// Per-connection: the action mailbox (in) and the frame batch
    /// receiver (out) — kept so the tests can send requests and read the
    /// private frames.
    actions: HashMap<ConnectionId, Mailbox<Action>>,
    outs: HashMap<ConnectionId, mpsc::Receiver<FrameBatch>>,
    /// The room's metrics channel, kept by the tests that read the
    /// room's own counters (the reject-bucket wiring tests).
    metrics_rx: mpsc::Receiver<MetricsEvent>,
}

impl Harness {
    async fn new(config: RoomConfig) -> Self {
        let (tick_tx, _first) = broadcast::channel(64);
        let tick_rx = tick_tx.subscribe();
        let (control, control_rx) = channel(config.control_capacity);
        // Capacity 64: enough for every step of the shortest tests, so
        // the bucket tests' `latest_room_sample` never reads a sample the
        // room had to drop (the other tests do not drain it — a full
        // channel just counts `metrics_dropped` inside the room, which
        // changes no behaviour they assert on).
        let (metrics_tx, metrics_rx) = mpsc::channel::<MetricsEvent>(64);
        let (ext_tx, resolvers) = mpsc::unbounded_channel::<Resolver>();
        let logic = RpcLogic { ext_tx };
        let actor = RoomActor::new(
            config,
            (),
            Box::new(logic),
            tick_rx,
            control_rx,
            1, // room rate == global rate: every tick is a step
            metrics_tx,
            None, // no result sink in these tests
        );
        Self {
            tick_tx,
            control,
            handle: tokio::spawn(actor.run()),
            t0: Instant::now(),
            next_tick: 0,
            resolvers,
            actions: HashMap::new(),
            outs: HashMap::new(),
            metrics_rx,
        }
    }

    fn tick(&mut self) {
        self.next_tick += 1;
        let at = self.t0 + Duration::from_secs_f64(self.next_tick as f64 * (1.0 / 30.0));
        self.tick_tx
            .send(TickInfo {
                tick: self.next_tick,
                at,
            })
            .expect("room subscriber alive");
    }

    /// Join `conn`, keeping its action mailbox + out receiver.
    async fn join(&mut self, conn: ConnectionId) {
        let (out_tx, out_rx) = mpsc::channel::<FrameBatch>(64);
        let (reply_tx, reply_rx) = oneshot::channel::<
            Result<(gsb_core::EntityId, Mailbox<Action>), gsb_core::CoreError>,
        >();
        self.control
            .send(RoomControl::Join {
                conn,
                out: out_tx,
                reply: reply_tx,
            })
            .await
            .expect("control alive");
        for _ in 0..2 {
            self.tick();
        }
        let (entity, actions) = tokio::time::timeout(Duration::from_secs(2), reply_rx)
            .await
            .expect("timed out waiting for join reply")
            .expect("join reply dropped")
            .expect("join accepted (room not full)");
        assert_eq!(entity, 1);
        self.actions.insert(conn, actions);
        self.outs.insert(conn, out_rx);
    }

    async fn leave(&mut self, conn: ConnectionId, entity: gsb_core::EntityId) {
        self.control
            .send(RoomControl::Leave { conn, entity })
            .await
            .expect("control alive");
        for _ in 0..2 {
            self.tick();
        }
    }

    /// Send one correlated request (the base-band envelope carrying the
    /// inner op + payload).
    async fn request(&mut self, conn: ConnectionId, id: u64, inner_op: u16, payload: &[u8]) {
        let env = base::RpcRequest {
            id,
            op: inner_op as u32,
            payload: payload.to_vec(),
        };
        let a = Action {
            conn,
            player: PlayerId(conn.0),
            op: gsb_core::rpc::RPC_REQ_OP,
            payload: env.encode_to_vec().into(),
        };
        self.actions
            .get(&conn)
            .expect("conn joined")
            .send(a)
            .await
            .expect("action mailbox alive");
    }

    /// Send a raw action with the envelope opcode but a malformed
    /// payload (decode failure).
    async fn raw_envelope(&mut self, conn: ConnectionId, payload: &[u8]) {
        let a = Action {
            conn,
            player: PlayerId(conn.0),
            op: gsb_core::rpc::RPC_REQ_OP,
            payload: payload.to_vec().into(),
        };
        self.actions
            .get(&conn)
            .expect("conn joined")
            .send(a)
            .await
            .expect("action mailbox alive");
    }

    /// Decode a batch's private frames into `[(id, ok)]` (the test
    /// format).
    fn decode_private(batch: &FrameBatch) -> Vec<(u64, bool)> {
        let mut replies = Vec::new();
        for f in batch {
            if f.op != OP_PRIV {
                continue;
            }
            for chunk in f.payload.chunks(9) {
                if chunk.len() < 9 {
                    continue;
                }
                let id = u64::from_le_bytes(chunk[..8].try_into().unwrap());
                replies.push((id, chunk[8] == 1));
            }
        }
        replies
    }

    /// Read the next private reply for `conn`, ticking while waiting: a
    /// worker's report needs one more tick's CONTROL phase to be
    /// reconciled (and task scheduling decides when the worker sends
    /// relative to the room's drain), so the read retries with a tick
    /// until `deadline`. Ticks while idle produce no batches, so
    /// retrying is safe.
    async fn wait_replies(&mut self, conn: ConnectionId, deadline: Duration) -> Vec<(u64, bool)> {
        let start = Instant::now();
        loop {
            let remaining = deadline.saturating_sub(start.elapsed());
            if remaining == Duration::ZERO {
                panic!("timed out waiting for a reply to {conn:?}");
            }
            let got = {
                let out = self.outs.get_mut(&conn).expect("out kept");
                tokio::time::timeout(remaining.min(Duration::from_millis(50)), out.recv()).await
            };
            match got {
                Err(_) => {
                    // No batch yet: one more tick (reconciles a pending
                    // worker report), then retry.
                    self.tick();
                }
                Ok(None) => panic!("out channel closed"),
                Ok(Some(batch)) => {
                    let replies = Self::decode_private(&batch);
                    if !replies.is_empty() {
                        return replies;
                    }
                    // A batch without private content: keep waiting.
                }
            }
        }
    }

    /// Read one batch from `conn`'s out channel (timeout-bounded) and
    /// decode its private frame into `[(id, ok)]` (the test format).
    async fn private_replies(&mut self, conn: ConnectionId, timeout: Duration) -> Vec<(u64, bool)> {
        let out = self.outs.get_mut(&conn).expect("out kept");
        let batch = tokio::time::timeout(timeout, out.recv())
            .await
            .expect("timed out waiting for a batch")
            .expect("out channel closed");
        let replies = Self::decode_private(&batch);
        assert!(
            !replies.is_empty(),
            "batch without a private frame (op {OP_PRIV:#04x})"
        );
        replies
    }

    /// Assert that no private frame for `conn` arrives within `timeout`:
    /// the leak check for "the other connection". The room ships a batch
    /// only when it has content, so the two passing shapes are "no batch
    /// at all" (the timeout) and "a batch without a private frame"
    /// (e.g. a snapshot — none in these tests, but the shape is the
    /// contract).
    async fn assert_no_private(&mut self, conn: ConnectionId, timeout: Duration) {
        let out = self.outs.get_mut(&conn).expect("out kept");
        let got = tokio::time::timeout(timeout, out.recv()).await;
        match got {
            Err(_) => {} // no batch at all: nothing leaked — correct
            Ok(Some(batch)) => {
                for f in &batch {
                    assert_ne!(f.op, OP_PRIV, "private frame leaked to {conn:?}");
                }
            }
            Ok(None) => panic!("out channel closed"),
        }
    }

    /// Take the next resolver pushed by the logic (an in-flight external
    /// request).
    async fn next_resolver(&mut self) -> Resolver {
        tokio::time::timeout(Duration::from_secs(2), self.resolvers.recv())
            .await
            .expect("timed out waiting for a resolver")
            .expect("resolvers channel closed")
    }

    /// The room's latest metrics sample (cumulative counters + gauges).
    /// Drains the (bounded) metrics channel and returns the newest
    /// `RoomSample`, keeping the buffer free so the room's per-step
    /// `try_send` never drops a sample while the test still runs.
    ///
    /// Synchronization contract: call this only right after a reply read
    /// (`private_replies` / `wait_replies`), with no newer tick sent in
    /// between. The reply batch is emitted by the step's BROADCAST phase
    /// and the step's metrics send is the LAST thing the (fully
    /// synchronous) step does — on this single-threaded test runtime the
    /// room task only yields at its next `tick_rx.recv()`, so by the time
    /// the reply read returns, the sample of that very step is in the
    /// buffer. The bucket tests use a config with a per-step metrics
    /// cadence (`bucket_cfg`), so "the latest sample" is "the sample of
    /// the step that just finished".
    fn latest_room_sample(&mut self) -> RoomSample {
        let mut latest = None;
        while let Ok(ev) = self.metrics_rx.try_recv() {
            if let MetricsEvent::Room(s) = ev {
                latest = Some(s);
            }
        }
        latest.expect("room has stepped at least once")
    }

    async fn shutdown(mut self) {
        let _ = self.control.send(RoomControl::Shutdown).await;
        for _ in 0..2 {
            self.tick();
        }
        self.handle.await.unwrap();
    }
}

fn cfg() -> RoomConfig {
    RoomConfig {
        id: RoomId(1),
        tick_hz: 30.0,
        ..Default::default()
    }
}

/// Config for the reject-bucket wiring tests: like `cfg()`, but with a
/// metrics cadence at the tick rate, so the room samples on EVERY step.
/// The tests read the room's counters through the metrics channel; at the
/// default 1 Hz cadence against a 30 Hz tick the room would sample only
/// every 30 steps, which a short test never reaches.
fn bucket_cfg() -> RoomConfig {
    RoomConfig {
        id: RoomId(1),
        tick_hz: 30.0,
        metrics_cadence_hz: 30.0,
        ..Default::default()
    }
}

/// Room-local request: answered in the SAME tick (one tick after the
/// request was sent), on the requesting connection's private frame only.
#[tokio::test]
async fn local_request_answered_same_tick_only_own_conn() {
    let mut h = Harness::new(cfg()).await;
    h.join(ConnectionId(1)).await;
    h.join(ConnectionId(2)).await;

    // Request on conn 1; a second, unrelated batch exists for conn 2 on
    // the same tick (the join's control already settled, so the only
    // content this tick can carry is the reply — to conn 1).
    h.request(ConnectionId(1), 7, OP_LOCAL, &[]).await;
    h.tick();

    let replies = h
        .private_replies(ConnectionId(1), Duration::from_secs(2))
        .await;
    assert_eq!(replies, vec![(7, true)]);

    // Conn 2 must NOT have received the reply: nothing private reached
    // it in this tick (the room ships a batch only when there is
    // content, so the leak check is "no private frame arrives").
    h.assert_no_private(ConnectionId(2), Duration::from_millis(150))
        .await;
    h.shutdown().await;
}

/// Room-local rejection (the logic said no): same tick, `ok = false`.
#[tokio::test]
async fn local_reject_same_tick() {
    let mut h = Harness::new(cfg()).await;
    h.join(ConnectionId(1)).await;
    h.request(ConnectionId(1), 3, OP_REJECT, &[]).await;
    h.tick();
    let replies = h
        .private_replies(ConnectionId(1), Duration::from_secs(2))
        .await;
    assert_eq!(replies, vec![(3, false)]);
    h.shutdown().await;
}

/// External request: the tick does NOT block on it (the room keeps
/// stepping and serving other connections while it is in flight), and
/// the answer arrives on a LATER tick through the same private path.
#[tokio::test]
async fn external_request_does_not_block_tick_answers_later_tick() {
    let mut h = Harness::new(cfg()).await;
    h.join(ConnectionId(1)).await;
    h.join(ConnectionId(2)).await;

    // Conn 1 starts an external request (the logic pushes its resolver;
    // the test keeps it UNRESOLVED).
    h.request(ConnectionId(1), 11, OP_EXT, &[]).await;
    h.tick();
    let _r = h.next_resolver().await; // in flight now

    // While conn 1's request is in flight, the room must keep ticking
    // and conn 2 must keep getting served (a local request answered on
    // the next tick) — the tick body never awaits the future.
    h.request(ConnectionId(2), 12, OP_LOCAL, &[]).await;
    h.tick();
    let replies2 = h
        .private_replies(ConnectionId(2), Duration::from_secs(2))
        .await;
    assert_eq!(replies2, vec![(12, true)]);

    // Conn 1 has no answer yet (still in flight, well before the
    // default 5 s deadline): nothing private reached it in the two
    // ticks above.
    h.assert_no_private(ConnectionId(1), Duration::from_millis(150))
        .await;

    // (The resolver is dropped with the harness; the worker exits on
    // the request's timeout — the room's sweep would have answered it
    // by then, which this harness never waits for.)
    h.shutdown().await;
}

/// The full external round trip: in flight for several ticks, then
/// resolved — the answer lands on the next tick with the matching id,
/// `ok = true`, and the payload the worker produced.
#[tokio::test]
async fn external_round_trip_later_tick() {
    let mut h = Harness::new(cfg()).await;
    h.join(ConnectionId(1)).await;

    h.request(ConnectionId(1), 21, OP_EXT, &[]).await;
    h.tick(); // registered pending + worker spawned
    let resolver = h.next_resolver().await; // worker started
    h.tick(); // still in flight
    h.tick(); // still in flight

    resolver
        .resolve
        .send(Ok(b"answer-bytes".to_vec()))
        .expect("resolver alive");
    // The report is reconciled on a later tick's CONTROL phase (the
    // read ticks while waiting).
    let replies = h
        .wait_replies(ConnectionId(1), Duration::from_secs(2))
        .await;
    assert_eq!(replies, vec![(21, true)]);
    h.shutdown().await;
}

/// An external worker that errors (the service rejected) → the answer
/// is a normal rejection reply (`ok = false`), not a violation and not a
/// hang.
#[tokio::test]
async fn external_error_is_normal_rejection() {
    let mut h = Harness::new(cfg()).await;
    h.join(ConnectionId(1)).await;
    h.request(ConnectionId(1), 31, OP_EXT, &[]).await;
    h.tick();
    let resolver = h.next_resolver().await;
    resolver
        .resolve
        .send(Err("out of stock".to_string()))
        .expect("resolver alive");
    // The report is reconciled on a later tick (the read ticks while
    // waiting).
    let replies = h
        .wait_replies(ConnectionId(1), Duration::from_secs(2))
        .await;
    assert_eq!(replies, vec![(31, false)]);
    h.shutdown().await;
}

/// The immediate external (resolves on first poll): still a LATER tick
/// (the worker is a task; its report is reconciled on the next CONTROL
/// phase) — the client-visible contract is "never the same tick".
#[tokio::test]
async fn external_immediate_still_later_tick() {
    let mut h = Harness::new(cfg()).await;
    h.join(ConnectionId(1)).await;
    h.request(ConnectionId(1), 41, OP_EXT_IMMEDIATE, &[]).await;
    h.tick(); // registered + worker spawned (it may already have reported)
    // Same-tick answer is impossible: the completion channel is drained
    // at the START of a tick, and the worker was spawned at its end —
    // so nothing private reaches conn 1 in this window.
    h.assert_no_private(ConnectionId(1), Duration::from_millis(150))
        .await;
    // The answer rides a later tick (the read ticks while waiting).
    let replies = h
        .wait_replies(ConnectionId(1), Duration::from_secs(2))
        .await;
    assert_eq!(replies, vec![(41, true)]);
    h.shutdown().await;
}

/// Timeout: a request whose worker never reports is swept by the room
/// (the client-visible authority) and answered with a timeout reply —
/// exactly ONE answer per request.
///
/// The worker's own timeout is a resource guard on the SAME deadline
/// (see the room's 2c): at the deadline it drops the future WITHOUT
/// reporting (the resolver's receiver dies with it — a late resolve is
/// structurally impossible, which is what makes "exactly one answer"
/// hold without any late-report window). The reconciliation drop of a
/// stale report is locked by `conn_close_in_flight_frees_slots_and_
/// drops_late_report` (the same `requests_late` path).
#[tokio::test]
async fn timeout_swept_exactly_one_answer() {
    let mut h = Harness::new(RoomConfig {
        id: RoomId(1),
        tick_hz: 30.0,
        request_timeout: Duration::from_millis(80),
        ..Default::default()
    })
    .await;
    h.join(ConnectionId(1)).await;

    h.request(ConnectionId(1), 51, OP_EXT, &[]).await;
    h.tick(); // pending (deadline ~80 ms of wall clock from here)
    let _resolver = h.next_resolver().await; // worker running; never resolved

    // A tick BEFORE the deadline (~50 ms in): no answer (the sweep
    // must not fire early).
    tokio::time::sleep(Duration::from_millis(50)).await;
    h.tick();
    h.assert_no_private(ConnectionId(1), Duration::from_millis(40))
        .await;

    // Wait past the deadline, then tick: the sweep answers it.
    tokio::time::sleep(Duration::from_millis(60)).await; // ~110 ms total
    h.tick();
    let replies = h
        .private_replies(ConnectionId(1), Duration::from_secs(2))
        .await;
    assert_eq!(replies, vec![(51, false)]);

    // Exactly one answer: the connection stays quiet afterwards (a
    // second answer would be a protocol breach; the worker's guard has
    // already exited without reporting, so none can come).
    h.assert_no_private(ConnectionId(1), Duration::from_millis(150))
        .await;
    h.shutdown().await;
}

/// A connection that leaves while a request is in flight: its pending
/// slots are freed (the caps are not leaked) and the late worker report
/// is dropped (no delivery to a gone session).
#[tokio::test]
async fn conn_close_in_flight_frees_slots_and_drops_late_report() {
    // Room-wide cap of 1: while conn 1's request is pending, conn 2's
    // external request must be rejected (room cap); after conn 1
    // leaves, conn 2's retry must register.
    let mut h = Harness::new(RoomConfig {
        id: RoomId(1),
        tick_hz: 30.0,
        max_pending_requests: 1,
        ..Default::default()
    })
    .await;
    h.join(ConnectionId(1)).await;
    h.join(ConnectionId(2)).await;

    h.request(ConnectionId(1), 61, OP_EXT, &[]).await;
    h.tick();
    let resolver1 = h.next_resolver().await;

    // Room cap: conn 2's request is rejected in the same tick (a local
    // reject: the reply rides this tick's broadcast, so a plain read
    // suffices — no worker report involved).
    h.request(ConnectionId(2), 62, OP_EXT, &[]).await;
    h.tick();
    let replies2 = h
        .private_replies(ConnectionId(2), Duration::from_secs(2))
        .await;
    assert_eq!(replies2, vec![(62, false)]);

    // Conn 1 leaves mid-flight: its slot is freed.
    h.leave(ConnectionId(1), 1).await;

    // Conn 2 retries: now accepted (registered pending).
    h.request(ConnectionId(2), 63, OP_EXT, &[]).await;
    h.tick();
    let resolver2 = h.next_resolver().await;
    assert_eq!(resolver2.id, 63);

    // The late report of conn 1's request arrives (conn 1 is gone): it
    // is dropped (no delivery, no panic, no slot accounting change).
    resolver1
        .resolve
        .send(Ok(b"stale".to_vec()))
        .expect("resolver alive");
    h.tick();
    h.tick();
    // Conn 2 is unaffected by the stale report: its own in-flight
    // request (id 63) is still pending — resolving IT now delivers
    // exactly one answer for it.
    resolver2
        .resolve
        .send(Ok(b"ok63".to_vec()))
        .expect("resolver alive");
    // Worker 63 is a separate task; its report lands on a later tick's
    // drain. `wait_replies` ticks while waiting, which is exactly what
    // the room needs to reconcile it (the read is the authority on
    // "the answer arrived").
    let replies2b = h
        .wait_replies(ConnectionId(2), Duration::from_secs(2))
        .await;
    assert_eq!(replies2b, vec![(63, true)]);
    h.shutdown().await;
}

/// Id-space rules: a duplicate id that is still in flight is rejected
/// WITHOUT re-processing; after the first answer, the same id is a
/// fresh request again (no unbounded history).
#[tokio::test]
async fn duplicate_inflight_id_rejected_then_reusable() {
    let mut h = Harness::new(cfg()).await;
    h.join(ConnectionId(1)).await;

    h.request(ConnectionId(1), 71, OP_EXT, &[]).await;
    h.tick();
    let resolver = h.next_resolver().await;

    // Duplicate while in flight: normal rejection, same tick.
    h.request(ConnectionId(1), 71, OP_LOCAL, &[]).await;
    h.tick();
    let replies = h
        .private_replies(ConnectionId(1), Duration::from_secs(2))
        .await;
    assert_eq!(replies, vec![(71, false)]);

    // Resolve the original: the answer for 71 arrives on a later tick
    // (worker report; the read ticks while waiting).
    resolver
        .resolve
        .send(Ok(vec![1, 2, 3]))
        .expect("resolver alive");
    let replies = h
        .wait_replies(ConnectionId(1), Duration::from_secs(2))
        .await;
    assert_eq!(replies, vec![(71, true)]);

    // The id is reusable now: a NEW request with the same id is
    // processed (room-local → answered same tick).
    h.request(ConnectionId(1), 71, OP_LOCAL, &[]).await;
    h.tick();
    let replies = h
        .private_replies(ConnectionId(1), Duration::from_secs(2))
        .await;
    assert_eq!(replies, vec![(71, true)]);
    h.shutdown().await;
}

/// Caps: the per-connection pending cap rejects the (N+1)-th in-flight
/// request of one connection while the room cap is still free.
#[tokio::test]
async fn per_conn_pending_cap() {
    let mut h = Harness::new(RoomConfig {
        id: RoomId(1),
        tick_hz: 30.0,
        max_pending_requests_per_conn: 2,
        ..Default::default()
    })
    .await;
    h.join(ConnectionId(1)).await;

    h.request(ConnectionId(1), 81, OP_EXT, &[]).await;
    h.request(ConnectionId(1), 82, OP_EXT, &[]).await;
    h.tick(); // both register (cap = 2)
    let _r1 = h.next_resolver().await;
    let _r2 = h.next_resolver().await;

    // The third: rejected in the same tick (per-connection cap).
    h.request(ConnectionId(1), 83, OP_EXT, &[]).await;
    h.tick();
    let replies = h
        .private_replies(ConnectionId(1), Duration::from_secs(2))
        .await;
    assert_eq!(replies, vec![(83, false)]);
    h.shutdown().await;
}

/// `id = 0` cannot correlate: a normal rejection, same tick.
#[tokio::test]
async fn id_zero_rejected() {
    let mut h = Harness::new(cfg()).await;
    h.join(ConnectionId(1)).await;
    h.request(ConnectionId(1), 0, OP_LOCAL, &[]).await;
    h.tick();
    let replies = h
        .private_replies(ConnectionId(1), Duration::from_secs(2))
        .await;
    assert_eq!(replies, vec![(0, false)]);
    h.shutdown().await;
}

/// A malformed envelope (garbage payload) is a normal rejection (a
/// client bug — the same class as an undecodable game payload), same
/// tick; the connection is not punished (no violation, the room keeps
/// serving it).
#[tokio::test]
async fn malformed_envelope_rejected() {
    let mut h = Harness::new(cfg()).await;
    h.join(ConnectionId(1)).await;
    h.raw_envelope(ConnectionId(1), &[0xFF, 0xFF, 0xFF]).await;
    h.tick();
    let replies = h
        .private_replies(ConnectionId(1), Duration::from_secs(2))
        .await;
    assert_eq!(replies, vec![(0, false)]);
    // The room still serves the connection (a well-formed request on
    // the next tick is answered normally).
    h.request(ConnectionId(1), 91, OP_LOCAL, &[]).await;
    h.tick();
    let replies = h
        .private_replies(ConnectionId(1), Duration::from_secs(2))
        .await;
    assert_eq!(replies, vec![(91, true)]);
    h.shutdown().await;
}

/// An op the logic does not handle → the core's normal "no handler"
/// rejection (the client learns immediately, instead of waiting for its
/// own timeout).
#[tokio::test]
async fn no_handler_rejected() {
    let mut h = Harness::new(cfg()).await;
    h.join(ConnectionId(1)).await;
    h.request(ConnectionId(1), 95, OP_UNKNOWN, &[]).await;
    h.tick();
    let replies = h
        .private_replies(ConnectionId(1), Duration::from_secs(2))
        .await;
    assert_eq!(replies, vec![(95, false)]);
    h.shutdown().await;
}

/// Ordering within a tick: fire-and-forget actions are processed before
/// the requests (the request sees the world after the tick's actions) —
/// here observable as: both arrive in one tick, the action is ingested
/// first (the logic would see the post-action world), and the request's
/// answer rides the same tick's broadcast.
#[tokio::test]
async fn actions_before_requests_same_tick() {
    let mut h = Harness::new(cfg()).await;
    h.join(ConnectionId(1)).await;
    // A plain (non-request) action and a request, in one tick: the
    // action goes to `ingest`, the request to `handle_request`, and the
    // reply is delivered in the same tick's broadcast.
    let plain = Action {
        conn: ConnectionId(1),
        player: PlayerId(1),
        op: 0x77,
        payload: bytes::Bytes::new(),
    };
    h.actions
        .get(&ConnectionId(1))
        .unwrap()
        .send(plain)
        .await
        .unwrap();
    h.request(ConnectionId(1), 101, OP_LOCAL, &[]).await;
    h.tick();
    let replies = h
        .private_replies(ConnectionId(1), Duration::from_secs(2))
        .await;
    assert_eq!(replies, vec![(101, true)]);
    h.shutdown().await;
}
