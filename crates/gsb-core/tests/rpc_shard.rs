//! The correlated-request (RPC) pattern invariants on the SHARDED path
//! (the Faz 3 promotion — the shard-side mirror of `rpc.rs`; see
//! `gsb_core::rpc` for where the pending state lives and why):
//!
//! - a room-local request is answered in the SAME tick, on the
//!   requesting connection's private frame ONLY (no leak);
//! - an external-I/O request does not block the tick; its answer arrives
//!   on a LATER tick through the same private path;
//! - a duplicate in-flight id is rejected WITHOUT re-processing, and the
//!   id becomes reusable once answered;
//! - the timeout sweep answers a stuck request with the TIMEOUT reason
//!   (exactly one answer per id);
//! - the per-connection pending cap rejects overflow in the same tick;
//! - a session that closes mid-flight frees its slots and its late
//!   worker report is harmlessly dropped (`requests_late`);
//! - the machinery is PER SHARD ACTOR: two connections homed on
//!   different shards of one logical room are answered independently,
//!   without cross-shard leaks;
//! - the shard's request counters flow into its metrics sample (no
//!   longer pinned to zero).
//!
//! The harness drives spawned [`ShardActor`]s off a manual broadcast
//! ticker — the `rpc.rs` idioms (manual ticks, bounded reads with
//! retry-and-tick, the `[id: u64 LE][ok: u8]` private encoding), adapted
//! to the shard control vocabulary ([`ShardMsg::Join`] carries the join
//! epoch; shutdown rides the same channel).

use std::collections::HashMap;
use std::time::{Duration, Instant};

use bytes::BufMut;
use gsb_core::channel::{FrameBatch, Mailbox, channel};
use gsb_core::id::{ConnectionId, PlayerId, RoomId};
use gsb_core::metrics::{MetricsEvent, RoomSample};
use gsb_core::room::{Action, Admission, GameLogic, RoomConfig, TickCtx};
use gsb_core::rpc::RequestDecision;
use gsb_core::shard::{ShardActor, ShardLogic, ShardMsg};
use gsb_core::ticker::TickInfo;
use gsb_protocol::base;
use prost::Message;
use tokio::sync::{broadcast, mpsc, oneshot};

// Test opcodes (inner game-band ops; the shard does not validate them —
// the logic decides). Same vocabulary as `rpc.rs`.
const OP_LOCAL: u16 = 0x01;
const OP_REJECT: u16 = 0x02;
const OP_EXT: u16 = 0x03;

const OP_SNAP: u16 = 0x1101;
const OP_PRIV: u16 = 0x1102;

#[path = "rpc_shard/paused.rs"]
mod paused;
#[path = "rpc_shard/unread.rs"]
mod unread;

/// One in-flight external request the test logic has delegated: its
/// correlation id and the resolver (sending `Ok` completes it, `Err`
/// rejects it; dropping it leaves the future pending until the shard's
/// timeout guard kills it).
struct Resolver {
    id: u64,
    resolve: oneshot::Sender<Result<Vec<u8>, String>>,
}

/// The test shard logic (the `rpc.rs::RpcLogic` shape over the sharding
/// seam): single snapshot group per shard, no migrations/borders, and
/// the same four-way request decision table. Private frames encode each
/// reply as `[id: u64 LE][ok: u8]`.
struct ShardRpcLogic {
    index: usize,
    ext_tx: mpsc::UnboundedSender<Resolver>,
}

impl GameLogic<()> for ShardRpcLogic {
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
        // Test identity policy: the conn id doubles as the player id.
        Admission {
            player: PlayerId(c.0),
            entity: c.0,
        }
    }
    fn on_leave(&mut self, _w: &mut (), _p: PlayerId) {}
    fn ingest(&mut self, _w: &mut (), _c: &TickCtx, _a: &mut Vec<Action>) {}
    fn update(&mut self, _w: &mut (), _c: &TickCtx) {}

    // Faz 3: the request seam lives on the shared `GameLogic` supertrait;
    // the shard actor drives it exactly like the room actor does.
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
                // The resolver is handed out from INSIDE the future (first
                // poll — i.e. only once the shard committed the
                // registration and spawned the worker); a cap/dup-rejected
                // External drops the future unpolled, so no orphan handle.
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
            _ => None,
        }
    }
}

impl ShardLogic<()> for ShardRpcLogic {
    type State = ();

    fn index(&self) -> usize {
        self.index
    }
    fn shard_count(&self) -> usize {
        2
    }
    fn serial_capacity(&self) -> u64 {
        gsb_core::shard::SHARD_SERIAL_CAPACITY
    }
    fn serial_used(&self) -> u64 {
        0
    }
    fn neighbors(&self) -> &[usize] {
        &[]
    }
    fn collect_migrations(
        &mut self,
        _w: &mut (),
        _nb: usize,
    ) -> Vec<gsb_core::shard::Migrating<()>> {
        Vec::new()
    }
    fn on_migrate_in(&mut self, _w: &mut (), _wire: u64, _state: (), _p: Option<PlayerId>) {}
    fn on_migrate_out(&mut self, _w: &mut (), _wire: u64) {}
    fn collect_border(&self, _w: &()) -> Vec<gsb_core::shard::BorderRecord<()>> {
        Vec::new()
    }
    fn own_wires(&self, _w: &()) -> Vec<u64> {
        Vec::new()
    }
}

struct ShardHandle {
    /// The shard's control mailbox (join/shutdown).
    tx: Mailbox<ShardMsg<(), ()>>,
    metrics_rx: mpsc::Receiver<MetricsEvent>,
    handle: tokio::task::JoinHandle<()>,
    resolvers: mpsc::UnboundedReceiver<Resolver>,
}

/// The harness: N independent shard actors (the registry spawns one per
/// shard; here the test owns them directly) off ONE manual broadcast
/// ticker. Joins go to a chosen shard; every shard keeps its own metrics
/// receiver (its sample id is `room << 16 | index`, so the channels never
/// need disambiguation).
struct Harness {
    tick_tx: broadcast::Sender<TickInfo>,
    shards: Vec<Option<ShardHandle>>,
    t0: Instant,
    next_tick: u64,
    actions: HashMap<(usize, ConnectionId), Mailbox<Action>>,
    outs: HashMap<(usize, ConnectionId), mpsc::Receiver<FrameBatch>>,
}

impl Harness {
    async fn new(n_shards: usize, config: RoomConfig) -> Self {
        let (tick_tx, _first) = broadcast::channel(64);
        let mut shards = Vec::with_capacity(n_shards);
        for i in 0..n_shards {
            let tick_rx = tick_tx.subscribe();
            let (tx, rx) = channel(config.control_capacity);
            let (metrics_tx, metrics_rx) = mpsc::channel::<MetricsEvent>(64);
            let (ext_tx, resolvers) = mpsc::unbounded_channel::<Resolver>();
            let actor = ShardActor::new(
                config.clone(),
                i,
                (),
                Box::new(ShardRpcLogic { index: i, ext_tx }),
                tick_rx,
                rx,
                vec![],
                1, // shard rate == global rate: every tick is a step
                metrics_tx,
                None, // no result sink in these tests
            );
            shards.push(Some(ShardHandle {
                tx,
                metrics_rx,
                handle: tokio::spawn(actor.run()),
                resolvers,
            }));
        }
        Self {
            tick_tx,
            shards,
            t0: Instant::now(),
            next_tick: 0,
            actions: HashMap::new(),
            outs: HashMap::new(),
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
            .expect("shard subscribers alive");
    }

    /// Join `conn` on `shard`, keeping its action mailbox + out receiver.
    async fn join(&mut self, shard: usize, conn: ConnectionId) {
        let (out_tx, out_rx) = mpsc::channel::<FrameBatch>(64);
        let (reply_tx, reply_rx) = oneshot::channel::<
            Result<(gsb_core::EntityId, Mailbox<Action>), gsb_core::CoreError>,
        >();
        let tx = self.shards[shard].as_ref().expect("shard alive").tx.clone();
        tx.send(ShardMsg::Join {
            conn,
            epoch: 1,
            identity: String::new(),
            out: out_tx,
            reply: reply_tx,
        })
        .await
        .expect("shard alive");
        for _ in 0..2 {
            self.tick();
        }
        let (_entity, actions) = tokio::time::timeout(Duration::from_secs(2), reply_rx)
            .await
            .expect("timed out waiting for join reply")
            .expect("join reply dropped")
            .expect("join accepted");
        self.actions.insert((shard, conn), actions);
        self.outs.insert((shard, conn), out_rx);
    }

    /// Send one correlated request (the base-band envelope carrying the
    /// inner op + payload).
    async fn request(&mut self, shard: usize, conn: ConnectionId, id: u64, inner_op: u16) {
        let env = base::RpcRequest {
            id,
            op: inner_op as u32,
            payload: Vec::new(),
        };
        let a = Action {
            conn,
            player: PlayerId(conn.0),
            op: gsb_core::rpc::RPC_REQ_OP,
            payload: env.encode_to_vec().into(),
        };
        self.actions
            .get(&(shard, conn))
            .expect("conn joined")
            .send(a)
            .await
            .expect("action mailbox alive");
    }

    /// Decode a batch's private frames into `[(id, ok)]` (the test format).
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

    /// Read one batch from the connection's out channel and decode its
    /// private frame into `[(id, ok)]`.
    async fn private_replies(
        &mut self,
        shard: usize,
        conn: ConnectionId,
        timeout: Duration,
    ) -> Vec<(u64, bool)> {
        let out = self.outs.get_mut(&(shard, conn)).expect("out kept");
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

    /// Read the next private reply for the connection, ticking while
    /// waiting: a worker report reconciles on a later tick's 0b phase,
    /// and task scheduling decides when the worker sends relative to the
    /// shard's drain. Ticks while idle produce no batches, so retrying is
    /// safe (same idiom as `rpc.rs::wait_replies`).
    async fn wait_replies(
        &mut self,
        shard: usize,
        conn: ConnectionId,
        deadline: Duration,
    ) -> Vec<(u64, bool)> {
        let start = Instant::now();
        loop {
            let remaining = deadline.saturating_sub(start.elapsed());
            if remaining == Duration::ZERO {
                panic!("timed out waiting for a reply to {conn:?}");
            }
            let got = {
                let out = self.outs.get_mut(&(shard, conn)).expect("out kept");
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
                }
            }
        }
    }

    /// Assert that no private frame reaches the connection within
    /// `timeout`: the leak check. A shipped batch without a private frame
    /// also passes (the shard ships only when there IS content, so the
    /// common shape is "no batch at all").
    async fn assert_no_private(&mut self, shard: usize, conn: ConnectionId, timeout: Duration) {
        let out = self.outs.get_mut(&(shard, conn)).expect("out kept");
        let got = tokio::time::timeout(timeout, out.recv()).await;
        match got {
            Err(_) => {}
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
    async fn next_resolver(&mut self, shard: usize) -> Resolver {
        let h = self.shards[shard].as_mut().expect("shard alive");
        tokio::time::timeout(Duration::from_secs(2), h.resolvers.recv())
            .await
            .expect("timed out waiting for a resolver")
            .expect("resolvers channel closed")
    }

    /// The shard's latest metrics sample (drains the bounded channel; see
    /// `rpc.rs::latest_room_sample` for the synchronization contract: call
    /// right after a reply read, with no newer tick in between).
    fn latest_sample(&mut self, shard: usize) -> RoomSample {
        let h = self.shards[shard].as_mut().expect("shard alive");
        let mut latest = None;
        while let Ok(ev) = h.metrics_rx.try_recv() {
            if let MetricsEvent::Room(s) = ev {
                latest = Some(s);
            }
        }
        latest.expect("shard has stepped at least once")
    }

    /// Leave `conn` (the registry's leave broadcast goes to ALL shards;
    /// exactly one matches the entity-id guard — here only the owner gets
    /// a matching row anyway).
    async fn leave(&mut self, shard: usize, conn: ConnectionId, entity: gsb_core::EntityId) {
        let tx = self.shards[shard].as_ref().expect("shard alive").tx.clone();
        tx.send(ShardMsg::Leave {
            conn,
            entity,
            epoch: 1,
        })
        .await
        .expect("shard alive");
        for _ in 0..2 {
            self.tick();
        }
    }

    async fn shutdown(mut self) {
        for s in self.shards.iter_mut().flatten() {
            let _ = s.tx.send(ShardMsg::Shutdown).await;
        }
        for _ in 0..2 {
            self.tick();
        }
        for s in self.shards.into_iter().flatten() {
            let _ = tokio::time::timeout(Duration::from_secs(2), s.handle).await;
        }
    }
}

fn cfg(id: u64) -> RoomConfig {
    RoomConfig {
        id: RoomId(id),
        tick_hz: 30.0,
        ..Default::default()
    }
}

/// Config with a per-step metrics cadence (counter-wiring tests).
fn bucket_cfg(id: u64) -> RoomConfig {
    RoomConfig {
        id: RoomId(id),
        tick_hz: 30.0,
        metrics_cadence_hz: 30.0,
        ..Default::default()
    }
}

/// Room-local request on a SHARDED room: answered in the SAME tick, on
/// the requesting connection's private frame only.
#[tokio::test]
async fn shard_local_request_answered_same_tick_only_own_conn() {
    let mut h = Harness::new(1, cfg(1)).await;
    h.join(0, ConnectionId(1)).await;
    h.join(0, ConnectionId(2)).await;

    h.request(0, ConnectionId(1), 7, OP_LOCAL).await;
    h.tick();

    let replies = h
        .private_replies(0, ConnectionId(1), Duration::from_secs(2))
        .await;
    assert_eq!(replies, vec![(7, true)]);

    // No leak: conn 2 sees nothing private this tick.
    h.assert_no_private(0, ConnectionId(2), Duration::from_millis(150))
        .await;
    h.shutdown().await;
}

/// External (delegated) request: the tick does NOT block; the answer
/// arrives on a LATER tick through the same private path, carrying the
/// worker's payload.
#[tokio::test]
async fn shard_external_reply_arrives_later_tick() {
    let mut h = Harness::new(1, cfg(1)).await;
    h.join(0, ConnectionId(1)).await;

    h.request(0, ConnectionId(1), 11, OP_EXT).await;
    h.tick(); // registered pending + worker spawned
    let resolver = h.next_resolver(0).await; // worker started
    h.tick(); // still in flight

    // While in flight, the shard keeps ticking and serving OTHER work
    // (a second connection's local request is answered meanwhile).
    h.join(0, ConnectionId(2)).await;
    h.request(0, ConnectionId(2), 12, OP_LOCAL).await;
    h.tick();
    let replies2 = h
        .private_replies(0, ConnectionId(2), Duration::from_secs(2))
        .await;
    assert_eq!(replies2, vec![(12, true)]);
    h.assert_no_private(0, ConnectionId(1), Duration::from_millis(150))
        .await;

    // Resolve: the report reconciles on a later tick's 0b phase.
    resolver
        .resolve
        .send(Ok(b"answer-bytes".to_vec()))
        .expect("resolver alive");
    let replies = h
        .wait_replies(0, ConnectionId(1), Duration::from_secs(2))
        .await;
    assert_eq!(replies, vec![(11, true)]);
    h.shutdown().await;
}

/// Id-space rules: a duplicate id still in flight is rejected WITHOUT
/// re-processing (even under a different inner op); once answered, the
/// id is fresh again (no unbounded history).
#[tokio::test]
async fn shard_duplicate_inflight_id_rejected_without_reprocessing() {
    let mut h = Harness::new(1, cfg(1)).await;
    h.join(0, ConnectionId(1)).await;

    h.request(0, ConnectionId(1), 71, OP_EXT).await;
    h.tick();
    let resolver = h.next_resolver(0).await;

    // Duplicate while in flight: normal rejection, same tick.
    h.request(0, ConnectionId(1), 71, OP_LOCAL).await;
    h.tick();
    let replies = h
        .private_replies(0, ConnectionId(1), Duration::from_secs(2))
        .await;
    assert_eq!(replies, vec![(71, false)]);

    // Resolve the original; its answer arrives on a later tick.
    resolver
        .resolve
        .send(Ok(vec![1, 2, 3]))
        .expect("resolver alive");
    let replies = h
        .wait_replies(0, ConnectionId(1), Duration::from_secs(2))
        .await;
    assert_eq!(replies, vec![(71, true)]);

    // The id is reusable now: a new request under the same id processes.
    h.request(0, ConnectionId(1), 71, OP_LOCAL).await;
    h.tick();
    let replies = h
        .private_replies(0, ConnectionId(1), Duration::from_secs(2))
        .await;
    assert_eq!(replies, vec![(71, true)]);
    h.shutdown().await;
}

/// The shard's request timeout is the client-visible authority: a stuck
/// future is swept on the tick past its deadline and answered with the
/// TIMEOUT reason — exactly ONE answer (the worker's resource guard has
/// exited without reporting, so none can come afterwards).
#[tokio::test]
async fn shard_timeout_sweep_answers_timeout_reason() {
    let mut h = Harness::new(
        1,
        RoomConfig {
            id: RoomId(1),
            tick_hz: 30.0,
            request_timeout: Duration::from_millis(80),
            ..Default::default()
        },
    )
    .await;
    h.join(0, ConnectionId(1)).await;

    h.request(0, ConnectionId(1), 51, OP_EXT).await;
    h.tick(); // pending (deadline ~80 ms of wall clock)
    let _resolver = h.next_resolver(0).await; // worker running; never resolved

    // Before the deadline: no answer (the sweep must not fire early).
    tokio::time::sleep(Duration::from_millis(50)).await;
    h.tick();
    h.assert_no_private(0, ConnectionId(1), Duration::from_millis(40))
        .await;

    // Past the deadline, the next tick sweeps it.
    tokio::time::sleep(Duration::from_millis(60)).await; // ~110 ms total
    h.tick();
    let replies = h
        .wait_replies(0, ConnectionId(1), Duration::from_secs(2))
        .await;
    assert_eq!(replies, vec![(51, false)]);

    // Exactly one answer: the connection stays quiet afterwards.
    h.assert_no_private(0, ConnectionId(1), Duration::from_millis(150))
        .await;
    h.shutdown().await;
}

/// Caps: the per-connection pending cap rejects the (N+1)-th in-flight
/// request of one connection in the SAME tick while the shard-wide cap is
/// nowhere near.
#[tokio::test]
async fn shard_per_conn_pending_cap_rejects_overflow() {
    let mut h = Harness::new(
        1,
        RoomConfig {
            id: RoomId(1),
            tick_hz: 30.0,
            max_pending_requests_per_conn: 2,
            ..Default::default()
        },
    )
    .await;
    h.join(0, ConnectionId(1)).await;

    h.request(0, ConnectionId(1), 81, OP_EXT).await;
    h.request(0, ConnectionId(1), 82, OP_EXT).await;
    h.tick(); // both register (cap = 2)
    let _r1 = h.next_resolver(0).await;
    let _r2 = h.next_resolver(0).await;

    // The third: rejected in the same tick (per-connection cap).
    h.request(0, ConnectionId(1), 83, OP_EXT).await;
    h.tick();
    let replies = h
        .private_replies(0, ConnectionId(1), Duration::from_secs(2))
        .await;
    assert_eq!(replies, vec![(83, false)]);
    h.shutdown().await;
}

/// A session that closes mid-flight: its pending slots are FREED (the
/// caps are not leaked) and its late worker report is harmlessly dropped
/// (counted `requests_late` — the exactly-one-answer reconciliation in
/// action; no delivery to a gone session, no slot accounting change for
/// the other connection).
#[tokio::test]
async fn shard_session_close_midflight_frees_slots_and_drops_late_report() {
    let mut h = Harness::new(
        1,
        bucket_cfg_with(RoomId(1), 1), // shard-wide cap 1
    )
    .await;
    h.join(0, ConnectionId(1)).await;
    h.join(0, ConnectionId(2)).await;

    h.request(0, ConnectionId(1), 61, OP_EXT).await;
    h.tick();
    let resolver1 = h.next_resolver(0).await;

    // The shard-wide cap (1): conn 2's external request is rejected now.
    h.request(0, ConnectionId(2), 62, OP_EXT).await;
    h.tick();
    let replies2 = h
        .private_replies(0, ConnectionId(2), Duration::from_secs(2))
        .await;
    assert_eq!(replies2, vec![(62, false)]);

    // Conn 1 closes mid-flight (entity id 1): its slot frees.
    h.leave(0, ConnectionId(1), 1).await;

    // Conn 2 retries: accepted now.
    h.request(0, ConnectionId(2), 63, OP_EXT).await;
    h.tick();
    let resolver2 = h.next_resolver(0).await;
    assert_eq!(resolver2.id, 63);

    // Conn 1's late report arrives after its session is gone: dropped.
    resolver1
        .resolve
        .send(Ok(b"stale".to_vec()))
        .expect("alive");
    h.tick();
    h.tick();

    // Conn 2 is unaffected: its own request resolves to exactly one answer.
    resolver2.resolve.send(Ok(b"ok63".to_vec())).expect("alive");
    let replies2b = h
        .wait_replies(0, ConnectionId(2), Duration::from_secs(2))
        .await;
    assert_eq!(replies2b, vec![(63, true)]);

    // The reconciliation counted the stale report exactly once.
    let s = h.latest_sample(0);
    assert_eq!(
        s.requests_late, 1,
        "the stale report was dropped and counted"
    );
    assert_eq!(
        s.pending_requests, 0,
        "both requests settled: the late one was dropped, conn 2's was \
         answered (the read above reconciled it)"
    );
    h.shutdown().await;
}

/// The machinery is PER SHARD ACTOR: two connections homed on different
/// shards of one logical room are answered independently — each reply
/// reaches only its owner, and neither shard serves the other's requests.
#[tokio::test]
async fn shard_requests_are_isolated_across_shards_of_one_room() {
    // Per-step metrics cadence: the cross-shard assertions read each
    // actor's counters right after its reply read.
    let mut h = Harness::new(2, bucket_cfg(1)).await;
    h.join(0, ConnectionId(1)).await;
    h.join(1, ConnectionId(2)).await;

    h.request(0, ConnectionId(1), 91, OP_LOCAL).await;
    h.request(1, ConnectionId(2), 92, OP_LOCAL).await;
    h.tick();

    let r1 = h
        .private_replies(0, ConnectionId(1), Duration::from_secs(2))
        .await;
    let r2 = h
        .private_replies(1, ConnectionId(2), Duration::from_secs(2))
        .await;
    assert_eq!(r1, vec![(91, true)]);
    assert_eq!(r2, vec![(92, true)]);

    // Cross-shard checks: each shard served ONLY its own member — the
    // per-shard samples account exactly one local answer each (a leak
    // across actors is structurally impossible — separate outbound
    // channels — so the load-bearing check is the independent
    // reconciliation each actor performed).
    let s0 = h.latest_sample(0);
    let s1 = h.latest_sample(1);
    assert_eq!(s0.requests_local, 1, "shard 0 answered only its member");
    assert_eq!(s1.requests_local, 1, "shard 1 answered only its member");
    h.shutdown().await;
}

/// The counters are wired (Faz 3 signpost closed): after known traffic
/// the shard's sample carries non-zero `requests_local` /
/// `requests_external`, the dup reject lands in ITS bucket, and the
/// in-flight gauge tracks the pending set.
#[tokio::test]
async fn shard_request_counters_flow_into_the_metrics_sample() {
    let mut h = Harness::new(1, bucket_cfg(1)).await;
    h.join(0, ConnectionId(1)).await;

    h.request(0, ConnectionId(1), 101, OP_EXT).await; // registers pending
    h.request(0, ConnectionId(1), 101, OP_LOCAL).await; // dup id: rejected
    h.tick();
    let replies = h
        .private_replies(0, ConnectionId(1), Duration::from_secs(2))
        .await;
    assert_eq!(replies, vec![(101, false)]);

    h.request(0, ConnectionId(1), 102, OP_LOCAL).await; // same-tick local
    h.tick();
    let replies = h
        .private_replies(0, ConnectionId(1), Duration::from_secs(2))
        .await;
    assert_eq!(replies, vec![(102, true)]);

    let s = h.latest_sample(0);
    assert_eq!(s.requests_external, 1);
    assert_eq!(s.requests_local, 1);
    assert_eq!(s.requests_rejected_dup, 1);
    assert_eq!(s.requests_rejected_malformed, 0);
    assert_eq!(s.requests_timed_out, 0);
    assert_eq!(s.requests_late, 0);
    assert_eq!(
        s.pending_requests, 1,
        "the external request stays in flight"
    );
    h.shutdown().await;
}

fn bucket_cfg_with(id: RoomId, room_cap: usize) -> RoomConfig {
    RoomConfig {
        id,
        tick_hz: 30.0,
        metrics_cadence_hz: 30.0,
        max_pending_requests: room_cap,
        ..Default::default()
    }
}
