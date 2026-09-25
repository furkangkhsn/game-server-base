//! Reconnect/detach core mechanics (`docs/RECONNECT.md` §12, items 1–8,
//! plus the pending-RPC rebind case): the tests lock the decisions the
//! design fixed —
//!
//! 1. a held disconnect keeps the entity, its world state, its snapshot
//!    presence and its room-cap slot (§4), while pulling no input and
//!    shipping no broadcast to the dead half (§3.2/§7);
//! 2. a resume swaps the new session's channels onto the LIVE entity and
//!    replies with the SAME wire id (§5); input flows again after the
//!    swap;
//! 3. a sharded resume broadcast is accepted by EXACTLY ONE shard (§6 —
//!    the park record exists on exactly one shard);
//! 4. a resume that follows the hold's end is rejected stale and falls
//!    through to a transparent fresh join (§5, §7);
//! 5. an expired grace despawns through `on_detach_expired` → the
//!    ordinary leave funnel, releasing the slot; the AI-handover arm keeps
//!    everything alive behind the `bot_fed` marker (Tur B's seam — locked
//!    behaviorally here);
//! 6. a combat-held hold extends while `may_release` vetoes and ends when
//!    it clears (§14.4's logic-veto arm);
//! 7. a double session supersedes: a parked identity resumes into the new
//!    session (the old detached entry is released); two LIVE sessions for
//!    one identity end with the newer one winning and the older socket
//!    closed with ERROR 9 (§5);
//! 8. room classes: joins to a retired id answer ERROR 12
//!    ([`gsb_core::CoreError::RoomRetired`]), and a PERSISTENT room is
//!    rebuilt after a panic even with `restart_on_panic = false` (§8).
//!
//! Harnesses follow the existing idioms: rooms are driven off a MANUAL
//! ticker feed with the metrics channel as the per-step barrier (see
//! `read_fairness.rs`), registry flows use the real ticker and the raw
//! `RegistryMsg` vocabulary (see `supervision.rs`), and the shard test
//! drives two [`gsb_core::shard::ShardActor`]s directly.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use gsb_core::channel::{FrameBatch, Mailbox, channel};
use gsb_core::conn::ConnIn;
use gsb_core::error::CoreError;
use gsb_core::id::{ConnectionId, EntityId, PlayerId, RoomId};
use gsb_core::metrics::MetricsEvent;
use gsb_core::registry::{BuiltRoom, Registry, RegistryMsg, RoomFactory, RoomStatus};
use gsb_core::room::{
    Action, Admission, Detach, ExpireTo, GameLogic, ResumeFound, RoomActor, RoomConfig,
    RoomControl, RoomLogic, TickCtx,
};
use gsb_core::shard::{BorderRecord, Migrating, ShardActor, ShardLogic};
use gsb_core::ticker::{TickInfo, Ticker};
use prost::Message as _;
use tokio::sync::{mpsc, oneshot};

const SNAPSHOT_OP: u16 = 0x7500;

/// Wire ids per shard when [`ParkLogic`] is driven as a [`ShardLogic`]
/// (the test's own id policy — `index * SHARD_SPAN + n`, so index 0
/// counts like a single room — bounded by the same span).
const SHARD_SPAN: u64 = 1000;

// =====================================================================
// Shared test logic: one struct covers every room-level case via knobs.
//
// World state = the conn→entity map itself (the entity set IS the world);
// snapshots encode the sorted entity ids as u64 LE each and ALWAYS emit,
// so every step ships exactly one parseable batch per connection.
// =====================================================================

#[derive(Debug, Clone, Copy)]
enum Ledg {
    Held(PlayerId),
    /// A tombstone for an ended hold (the retention is game policy).
    Ended,
}

struct ParkLogic {
    next_id: u64,
    player_entity: HashMap<PlayerId, EntityId>,
    /// Per-player detach policy (set by the test before the detach; the
    /// test identity policy is "the conn id doubles as the player id").
    policy: HashMap<PlayerId, Detach>,
    /// Wildcard policy for connections without an explicit entry (used by
    /// the registry-driven tests, whose factory cannot know conn ids).
    hold_default: bool,
    /// The grace the wildcard policy hands out. Effectively "never expires"
    /// by default (the supersedence tests need the park to outlive them);
    /// the park-expiry tests shorten it so the sweep actually fires.
    hold_grace: Duration,
    /// Shard index, used only when this logic is driven as a
    /// [`ShardLogic`]. It offsets the minted wire ids into a per-shard
    /// range; index 0 (the default, and what every single-room test uses)
    /// leaves them exactly as they were.
    index: usize,
    /// The park ledger (§4: it lives in the LOGIC; the core only queries).
    ledger: HashMap<String, Ledg>,
    /// The `may_release` answer (flipped by the test). `true` by default —
    /// the trait's own default, "no veto": since the veto is also asked
    /// at a TIMED hold's deadline (RECONNECT §14.4), a fixture that
    /// vetoed by default would hold every timed park past its grace.
    release_ok: bool,
    /// Observed ingested ops: (player, op) — proves input does or does
    /// not flow (and WHO it was attributed to).
    ops: mpsc::Sender<(PlayerId, u16)>,
    /// External-request slots: each delegated request hands the TEST a
    /// oneshot the test fires whenever it wants (deterministic late
    /// completions).
    req_slots: mpsc::Sender<oneshot::Sender<Result<bytes::Bytes, String>>>,
}

impl ParkLogic {
    fn new(ops: mpsc::Sender<(PlayerId, u16)>) -> Self {
        let (slot_tx, _) = mpsc::channel(4);
        Self {
            next_id: 0,
            player_entity: HashMap::new(),
            policy: HashMap::new(),
            hold_default: false,
            hold_grace: Duration::from_secs(3600),
            index: 0,
            ledger: HashMap::new(),
            release_ok: true,
            ops,
            req_slots: slot_tx,
        }
    }
}

impl GameLogic<()> for ParkLogic {
    type GroupKey = ();
    type Strip = ();

    fn snapshot_op(&self) -> u16 {
        SNAPSHOT_OP
    }
    fn private_op(&self) -> u16 {
        0x7501
    }
    fn group_of(&self, _w: &(), _p: PlayerId) -> Self::GroupKey {}

    fn snapshot(
        &mut self,
        _w: &mut (),
        _c: &TickCtx,
        _g: &Self::GroupKey,
        _borrowed: &[gsb_core::shard::BorderRecord<()>],
        out: &mut bytes::BytesMut,
    ) -> bool {
        let mut ids: Vec<EntityId> = self.player_entity.values().copied().collect();
        ids.sort_unstable();
        for id in ids {
            out.extend_from_slice(&id.to_le_bytes());
        }
        true
    }

    fn on_join(&mut self, _w: &mut (), c: ConnectionId) -> Admission {
        self.next_id += 1;
        // Test identity policy: the conn id doubles as the player id, so
        // tests can address players by the conn they joined with.
        let player = PlayerId(c.0);
        // Wire ids live in this shard's span (index 0 = the identity
        // offset every single-room test already relies on).
        let entity = self.index as u64 * SHARD_SPAN + self.next_id;
        self.player_entity.insert(player, entity);
        Admission { player, entity }
    }

    fn on_leave(&mut self, _w: &mut (), player: PlayerId) {
        self.player_entity.remove(&player);
    }

    // -- the reconnect surface -----------------------------------------

    fn on_disconnect(&mut self, _w: &mut (), player: PlayerId, identity: &str) -> Detach {
        let decision = self
            .policy
            .get(&player)
            .copied()
            .or_else(|| {
                self.hold_default.then_some(Detach::Hold {
                    grace: Some(self.hold_grace),
                    to: ExpireTo::Despawn,
                })
            })
            .unwrap_or(Detach::Despawn);
        if let Detach::Hold { .. } = decision
            && self.player_entity.contains_key(&player)
        {
            self.ledger.insert(identity.to_string(), Ledg::Held(player));
        }
        decision
    }

    fn may_release(&mut self, _w: &mut (), _p: PlayerId) -> bool {
        self.release_ok
    }

    fn on_detach_expired(&mut self, _w: &mut (), _p: PlayerId, _to: ExpireTo) {
        // An ended hold leaves its record ENDED (not deleted): this is what
        // makes a later resume provably STALE instead of merely unknown.
        // Retention is game policy; this demo policy keeps the tombstone.
        for v in self.ledger.values_mut() {
            if matches!(*v, Ledg::Held(_)) {
                *v = Ledg::Ended;
            }
        }
    }

    fn resume_lookup(&self, _w: &(), identity: &str) -> ResumeFound {
        match self.ledger.get(identity) {
            Some(Ledg::Held(p)) => ResumeFound::Held(*p),
            Some(Ledg::Ended) => ResumeFound::Ended,
            None => ResumeFound::Never,
        }
    }

    fn on_resume(
        &mut self,
        _w: &mut (),
        identity: &str,
        _conn: ConnectionId,
        _player: PlayerId,
        _entity: EntityId,
    ) {
        // Consume the ledger entry. Nothing else to move: every table is
        // keyed by the STABLE player id (Faz 2), which the resume did not
        // change.
        self.ledger.remove(identity);
    }

    fn ingest(&mut self, _w: &mut (), _ctx: &TickCtx, actions: &mut Vec<Action>) {
        for a in actions.drain(..) {
            let _ = self.ops.try_send((a.player, a.op));
            // Test-only control op: clears the combat veto (delivered by a
            // LIVE session — a detached row's input is skipped by design).
            if a.op == 0xFFFF {
                self.release_ok = true;
            }
        }
    }

    fn update(&mut self, _w: &mut (), _ctx: &TickCtx) {}

    // Faz 3: the request seam lives on the shared `GameLogic` supertrait.
    fn handle_request(
        &mut self,
        _w: &mut (),
        _ctx: &TickCtx,
        req: &gsb_core::rpc::RpcRequest,
    ) -> Option<gsb_core::rpc::RequestDecision> {
        if req.op != 0x2001 {
            return None;
        }
        // Defer the answer until the TEST fires the oneshot it received:
        // a deterministic "external dependency still in flight".
        let (tx, rx) = oneshot::channel::<Result<bytes::Bytes, String>>();
        let _ = self.req_slots.try_send(tx);
        Some(gsb_core::rpc::RequestDecision::External(Box::pin(
            async move { rx.await.unwrap_or(Err("cancelled".into())) },
        )))
    }
}

// Faz 3 promotion: `handle_request` moved to `GameLogic`; this impl stays
// as the single-room marker.
impl RoomLogic<()> for ParkLogic {}

// The same logic driven as a SHARD, so the park mechanics (and the tests
// that exercise them) do not need a second implementation. Nothing here
// migrates or borders — the shard cases in this file are about the park
// lifecycle, not about the seam.
impl ShardLogic<()> for ParkLogic {
    type State = ();

    fn index(&self) -> usize {
        self.index
    }
    fn shard_count(&self) -> usize {
        2
    }
    fn serial_capacity(&self) -> u64 {
        SHARD_SPAN
    }
    fn serial_used(&self) -> u64 {
        self.next_id
    }
    fn neighbors(&self) -> &[usize] {
        &[]
    }
    fn collect_migrations(&mut self, _w: &mut (), _nb: usize) -> Vec<Migrating<()>> {
        Vec::new()
    }
    fn on_migrate_in(&mut self, _w: &mut (), _wire: u64, _state: (), _player: Option<PlayerId>) {}
    fn on_migrate_out(&mut self, _w: &mut (), _wire: u64) {}
    fn collect_border(&self, _w: &()) -> Vec<BorderRecord<()>> {
        Vec::new()
    }
    fn own_wires(&self, _w: &()) -> Vec<u64> {
        self.player_entity.values().copied().collect()
    }
}

// =====================================================================
// Manual-ticker room harness (read_fairness.rs idiom): the metrics
// channel carries one sample per step, so a recv is the step barrier.
// =====================================================================

struct RoomH {
    tick_tx: tokio::sync::broadcast::Sender<TickInfo>,
    control: Mailbox<RoomControl>,
    handle: tokio::task::JoinHandle<()>,
    metrics_rx: mpsc::Receiver<MetricsEvent>,
    t0: Instant,
    next_tick: u64,
}

impl RoomH {
    fn new(config: RoomConfig, logic: ParkLogic) -> Self {
        let (tick_tx, tick_rx) = tokio::sync::broadcast::channel(256);
        let (control, control_rx) = channel(config.control_capacity.max(64));
        let (metrics_tx, metrics_rx) = mpsc::channel(1024);
        let actor = RoomActor::new(
            config,
            (),
            Box::new(logic),
            tick_rx,
            control_rx,
            1,
            metrics_tx,
            None,
        );
        let mut h = Self {
            tick_tx,
            control,
            handle: tokio::spawn(actor.run()),
            metrics_rx,
            t0: Instant::now(),
            next_tick: 0,
        };
        h.tick();
        h
    }

    fn tick(&mut self) {
        self.next_tick += 1;
        let at = self.t0 + Duration::from_secs_f64(self.next_tick as f64 / 60.0);
        self.tick_tx
            .send(TickInfo {
                tick: self.next_tick,
                at,
            })
            .expect("room subscribed");
    }

    /// Feed one tick and wait for its sample (the step-completed barrier).
    async fn step(&mut self) {
        self.tick();
        tokio::time::timeout(Duration::from_secs(5), self.metrics_rx.recv())
            .await
            .expect("step barrier timed out")
            .expect("metrics closed");
    }

    async fn steps(&mut self, n: usize) {
        for _ in 0..n {
            self.step().await;
        }
    }

    /// The freshest counters: run one more barriered step, then take its
    /// sample (counters are cumulative, so one extra step costs nothing).
    async fn latest_sample(&mut self) -> gsb_core::metrics::RoomSample {
        self.step().await;
        while self.metrics_rx.try_recv().is_ok() {}
        self.tick();
        let ev = tokio::time::timeout(Duration::from_secs(5), self.metrics_rx.recv())
            .await
            .expect("sample timed out")
            .expect("metrics closed");
        match ev {
            MetricsEvent::Room(s) => s,
            other => panic!("unexpected metrics event {other:?}"),
        }
    }

    async fn join(
        &mut self,
        conn: ConnectionId,
    ) -> (EntityId, Mailbox<Action>, mpsc::Receiver<FrameBatch>) {
        let (out_tx, out_rx) = mpsc::channel::<FrameBatch>(64);
        let (reply_tx, reply_rx) =
            oneshot::channel::<Result<(EntityId, Mailbox<Action>), CoreError>>();
        self.control
            .send(RoomControl::Join {
                conn,
                out: out_tx,
                reply: reply_tx,
            })
            .await
            .expect("control alive");
        self.step().await;
        let (entity, actions) = reply_rx
            .await
            .expect("reply dropped")
            .expect("join accepted");
        (entity, actions, out_rx)
    }

    async fn detach(&mut self, conn: ConnectionId, entity: EntityId, identity: &str) {
        self.control
            .send(RoomControl::Detach {
                conn,
                entity,
                identity: identity.to_string(),
            })
            .await
            .expect("control alive");
        self.step().await;
    }

    async fn resume(
        &mut self,
        conn: ConnectionId,
        epoch: u64,
        identity: &str,
    ) -> Result<(EntityId, Mailbox<Action>), CoreError> {
        let (out_tx, _out_rx) = mpsc::channel::<FrameBatch>(64);
        let (reply_tx, reply_rx) =
            oneshot::channel::<Result<(EntityId, Mailbox<Action>), CoreError>>();
        self.control
            .send(RoomControl::Resume {
                conn,
                epoch,
                identity: identity.to_string(),
                out: out_tx,
                reply: reply_tx,
            })
            .await
            .expect("control alive");
        self.step().await;
        tokio::time::timeout(Duration::from_secs(5), reply_rx)
            .await
            .expect("resume reply timed out")
            .expect("reply dropped")
    }

    /// Decode the newest snapshot batch in the queue into the entity set.
    async fn entities(out_rx: &mut mpsc::Receiver<FrameBatch>) -> Vec<EntityId> {
        let mut last = Vec::new();
        while let Ok(batch) = out_rx.try_recv() {
            for frame in batch {
                if frame.op == SNAPSHOT_OP {
                    last = frame
                        .payload
                        .chunks_exact(8)
                        .map(|c| u64::from_le_bytes(c.try_into().unwrap()))
                        .collect();
                }
            }
        }
        last
    }

    async fn shutdown(mut self) {
        let _ = self.control.send(RoomControl::Shutdown).await;
        self.tick();
        tokio::time::timeout(Duration::from_secs(5), self.handle)
            .await
            .expect("room did not shut down")
            .expect("room panicked");
    }
}

fn park_config(id: RoomId, cap: Option<usize>) -> RoomConfig {
    RoomConfig {
        id,
        tick_hz: 60.0,
        keepalive_hz: 0.0,
        // One sample per step: the harness's step barrier.
        metrics_cadence_hz: 60.0,
        max_players: cap,
        ..Default::default()
    }
}

async fn drain_ops(ops: &mut mpsc::Receiver<(PlayerId, u16)>) -> Vec<(PlayerId, u16)> {
    let mut v = Vec::new();
    while let Ok(x) = ops.try_recv() {
        v.push(x);
    }
    v
}

fn hold_forever() -> Detach {
    Detach::Hold {
        grace: Some(Duration::from_secs(3600)),
        to: ExpireTo::Despawn,
    }
}

// =====================================================================
// 1 — §12.1 disconnect_with_hold_keeps_entity_and_slot
// =====================================================================

#[tokio::test]
async fn disconnect_with_hold_keeps_entity_and_slot() {
    let (ops_tx, mut ops_rx) = mpsc::channel(64);
    let mut logic = ParkLogic::new(ops_tx.clone());
    logic.policy.insert(PlayerId(1), hold_forever());
    // Cap 2 = player + observer: the THIRD join attempt doubles as the
    // crisp slot proof while the park holds one of the two slots.
    let mut h = RoomH::new(park_config(RoomId(1), Some(2)), logic);

    let (entity, actions, _own_out) = h.join(ConnectionId(1)).await;
    // A LIVE observer: the detached row ships nothing BY DESIGN (its
    // outbound half is dead), so the room's snapshot content is observed
    // through someone else's fan-out.
    let (obs_ent, _obs_actions, mut obs_rx) = h.join(ConnectionId(2)).await;
    h.steps(1).await;
    assert_eq!(
        RoomH::entities(&mut obs_rx).await,
        vec![entity, obs_ent],
        "both members are in the world"
    );

    // Transport death → policy holds: row, entity AND cap slot stay.
    h.detach(ConnectionId(1), entity, "ana").await;

    // Slot proof: two rows exist (one parked), the cap binds the third.
    let (out_tx, _o) = mpsc::channel(8);
    let (rtx, rrx) = oneshot::channel();
    h.control
        .send(RoomControl::Join {
            conn: ConnectionId(3),
            out: out_tx,
            reply: rtx,
        })
        .await
        .unwrap();
    h.step().await;
    let err = rrx.await.expect("reply").expect_err("cap must reject");
    assert!(matches!(err, CoreError::RoomFull(_)), "got {err:?}");

    // World-state proof: the parked entity keeps appearing in the SNAPSHOT
    // other clients receive…
    h.steps(3).await;
    assert_eq!(
        RoomH::entities(&mut obs_rx).await,
        vec![entity, obs_ent],
        "parked entity stays in the world"
    );
    // …and the gauges still count the held slot.
    let s = h.latest_sample().await;
    assert_eq!(s.members, 2, "the park keeps both rows counted");
    assert_eq!(s.detached, 1, "one instant park");

    // Input proof: input sent to the OLD channel is never pulled.
    actions
        .send(Action {
            conn: ConnectionId(1),
            player: PlayerId(1),
            op: 0x1111,
            payload: bytes::Bytes::new(),
        })
        .await
        .unwrap();
    h.steps(3).await;
    assert!(
        !drain_ops(&mut ops_rx)
            .await
            .iter()
            .any(|(_, op)| *op == 0x1111),
        "no READ pulls for a detached row"
    );

    h.shutdown().await;
}

// =====================================================================
// 2 — §12.2 resume_binds_new_channels_to_live_entity
// =====================================================================

#[tokio::test]
async fn resume_binds_new_channels_to_live_entity() {
    let (ops_tx, mut ops_rx) = mpsc::channel(64);
    let mut logic = ParkLogic::new(ops_tx.clone());
    logic.policy.insert(PlayerId(1), hold_forever());
    let mut h = RoomH::new(park_config(RoomId(2), None), logic);

    let (entity, _old_actions, mut out_rx) = h.join(ConnectionId(1)).await;
    h.detach(ConnectionId(1), entity, "ana").await;

    // The implicit resume: SAME wire id comes back.
    let (entity2, new_actions) = h
        .resume(ConnectionId(9), 7, "ana")
        .await
        .expect("resume accepted");
    assert_eq!(entity2, entity, "wire id unchanged across the swap");

    // Input flows again — over the NEW channel.
    new_actions
        .send(Action {
            conn: ConnectionId(9),
            player: PlayerId(9),
            op: 0x2222,
            payload: bytes::Bytes::new(),
        })
        .await
        .unwrap();
    h.steps(2).await;
    let seen = drain_ops(&mut ops_rx).await;
    // Faz 2: the input is pulled AND attributed to the SAME STABLE player
    // the first session minted (PlayerId(1), from conn 1's join) — not to
    // a new identity per connection. The wire id equality above and this
    // attribution together are the resume contract.
    assert!(
        seen.contains(&(PlayerId(1), 0x2222)),
        "post-swap input is pulled, attributed to the SAME player the \
         first session minted: {seen:?}"
    );
    assert!(
        !seen.iter().any(|(p, _)| *p != PlayerId(1)),
        "no other player exists in this room: {seen:?}"
    );

    // Broadcasts flow again; the world view (entity set) did not jump.
    h.steps(1).await;
    assert_eq!(
        RoomH::entities(&mut out_rx).await,
        vec![entity],
        "other clients' view unchanged"
    );

    let s = h.latest_sample().await;
    assert_eq!(s.resumes, 1, "exactly one accepted resume");
    assert_eq!(s.detached, 0, "the park was consumed");

    h.shutdown().await;
}

// =====================================================================
// Faz 2 locks — the binding table is the ingest authority, and the
// player identity is stable across disconnect/resume cycles.
// =====================================================================

/// An action arriving under the OLD session's ConnectionId AFTER a
/// resume is not ingested: the rebind removed the old binding row, and
/// the binding is what translates `Action { conn }` at ingest. The new
/// session's actions (same room, same tick) ARE ingested — under the
/// SAME stable player id the first join minted.
#[tokio::test]
async fn actions_from_the_old_session_are_dropped_after_resume() {
    let (ops_tx, mut ops_rx) = mpsc::channel(64);
    let mut logic = ParkLogic::new(ops_tx.clone());
    logic.policy.insert(PlayerId(1), hold_forever());
    let mut h = RoomH::new(park_config(RoomId(43), None), logic);

    // Session A: conn 1 joins (mints PlayerId(1)), then dies; conn 9
    // resumes onto its row.
    let (_entity, _old_actions, _out) = h.join(ConnectionId(1)).await;
    h.detach(ConnectionId(1), _entity, "ana").await;
    let (entity2, new_actions) = h
        .resume(ConnectionId(9), 7, "ana")
        .await
        .expect("resume accepted");
    assert_eq!(entity2, _entity, "wire id unchanged");

    // One frame claiming the OLD conn and one claiming the NEW conn, in
    // the same tick's pull (the new channel is the only live path — this
    // is exactly how a stale/late frame would arrive).
    new_actions
        .send(Action {
            conn: ConnectionId(1),
            player: gsb_core::PlayerId(0), // placeholder: room stamps it
            op: 0x1111,
            payload: bytes::Bytes::new(),
        })
        .await
        .unwrap();
    new_actions
        .send(Action {
            conn: ConnectionId(9),
            player: gsb_core::PlayerId(0),
            op: 0x2222,
            payload: bytes::Bytes::new(),
        })
        .await
        .unwrap();
    h.steps(2).await;

    let seen = drain_ops(&mut ops_rx).await;
    assert!(
        !seen.iter().any(|(_, op)| *op == 0x1111),
        "the old session's action must not reach the world: {seen:?}"
    );
    assert!(
        seen.contains(&(PlayerId(1), 0x2222)),
        "the new session's action flows, attributed to the stable \
         player: {seen:?}"
    );

    h.shutdown().await;
}

/// PlayerId stability across REPEATED disconnect/resume cycles: every
/// resumed session keeps minting actions under the identity minted at
/// the FIRST join (and the wire id stays constant too). This is the
/// room-side half of the stability contract; the migration half lives
/// in the shard tests (`player_identity_is_stable_across_migration`).
#[tokio::test]
async fn player_identity_stable_across_disconnect_resume_cycles() {
    let (ops_tx, mut ops_rx) = mpsc::channel(64);
    let mut logic = ParkLogic::new(ops_tx.clone());
    logic.policy.insert(PlayerId(1), hold_forever());
    let mut h = RoomH::new(park_config(RoomId(44), None), logic);

    let (entity0, _a, _o) = h.join(ConnectionId(1)).await;
    for cycle in [2u64, 3, 4] {
        h.detach(ConnectionId(cycle - 1), entity0, "ana").await;
        let (entity, actions) = h
            .resume(ConnectionId(cycle), cycle, "ana")
            .await
            .expect("resume accepted");
        assert_eq!(entity, entity0, "wire id stable across cycle {cycle}");

        actions
            .send(Action {
                conn: ConnectionId(cycle),
                player: gsb_core::PlayerId(0),
                op: 0x3300 + cycle as u16,
                payload: bytes::Bytes::new(),
            })
            .await
            .unwrap();
        h.steps(2).await;
    }

    // Every cycle's input landed under ONE AND THE SAME player id — the
    // one minted at the first join — across three sessions.
    let seen = drain_ops(&mut ops_rx).await;
    for op in [0x3302u16, 0x3303, 0x3304] {
        assert!(
            seen.contains(&(PlayerId(1), op)),
            "cycle op {op:#x} ingested under the stable player: {seen:?}"
        );
    }
    assert!(
        seen.iter().all(|(p, _)| *p == PlayerId(1)),
        "no identity churn across resumes: {seen:?}"
    );

    h.shutdown().await;
}

// =====================================================================
// 4 — §12.4 stale_resume_rejected_after_expire
// =====================================================================

#[tokio::test]
async fn stale_resume_rejected_after_expire() {
    let (ops_tx, _ops) = mpsc::channel(64);
    let mut logic = ParkLogic::new(ops_tx);
    logic.policy.insert(
        PlayerId(1),
        Detach::Hold {
            grace: Some(Duration::from_millis(80)),
            to: ExpireTo::Despawn,
        },
    );
    let mut h = RoomH::new(park_config(RoomId(3), None), logic);

    let (entity, _a, _own_out) = h.join(ConnectionId(1)).await;
    let (obs_ent, _a2, mut obs_rx) = h.join(ConnectionId(2)).await;
    h.detach(ConnectionId(1), entity, "ana").await;

    // Outlive the grace, then let the sweep fire: only the observer's own
    // record remains in the world.
    tokio::time::sleep(Duration::from_millis(220)).await;
    h.steps(2).await;
    assert_eq!(
        RoomH::entities(&mut obs_rx).await,
        vec![obs_ent],
        "grace expiry despawned the parked entity"
    );

    // The late resume: mechanism-rejected (stale), client-transparent —
    // the fallback fresh join succeeds with a NEW wire id (§5).
    let (entity2, _a2b) = h
        .resume(ConnectionId(9), 9, "ana")
        .await
        .expect("fallback join ok");
    assert_ne!(entity2, entity, "a fresh entity, not a resurrection");

    let s = h.latest_sample().await;
    assert_eq!(s.resume_rejected_stale, 1, "the stale attempt is counted");

    h.shutdown().await;
}

// =====================================================================
// 5 — §12.5 grace_expiry_falls_back_to_despawn (+ AI stub variant)
// =====================================================================

#[tokio::test]
async fn grace_expiry_falls_back_to_despawn() {
    let (ops_tx, _ops) = mpsc::channel(64);
    let mut logic = ParkLogic::new(ops_tx);
    logic.policy.insert(
        PlayerId(1),
        Detach::Hold {
            grace: Some(Duration::from_millis(80)),
            to: ExpireTo::Despawn,
        },
    );
    // Cap 2 = player + observer: held while parked, freed after expiry.
    let mut h = RoomH::new(park_config(RoomId(4), Some(2)), logic);

    let (entity, _a, _own_out) = h.join(ConnectionId(1)).await;
    let (obs_ent, _a2, mut obs_rx) = h.join(ConnectionId(2)).await;
    h.detach(ConnectionId(1), entity, "ana").await;

    // While parked the slot is held: this join must bounce.
    let (out_tx, _o) = mpsc::channel(8);
    let (rtx, rrx) = oneshot::channel();
    h.control
        .send(RoomControl::Join {
            conn: ConnectionId(3),
            out: out_tx,
            reply: rtx,
        })
        .await
        .unwrap();
    h.step().await;
    assert!(
        rrx.await.expect("reply").is_err(),
        "slot must be held while parked"
    );

    tokio::time::sleep(Duration::from_millis(220)).await;
    h.steps(2).await;
    assert_eq!(
        RoomH::entities(&mut obs_rx).await,
        vec![obs_ent],
        "only the despawned player's record left the world"
    );

    // After expiry the SAME join fits: the sweep released the slot.
    let (out_tx, _o) = mpsc::channel(8);
    let (rtx, rrx) = oneshot::channel();
    h.control
        .send(RoomControl::Join {
            conn: ConnectionId(3),
            out: out_tx,
            reply: rtx,
        })
        .await
        .unwrap();
    h.step().await;
    assert!(
        rrx.await.expect("reply").is_ok(),
        "expiry released the cap slot"
    );

    let s = h.latest_sample().await;
    assert_eq!(s.detach_expired_despawn, 1);

    h.shutdown().await;
}

/// The AiHandover arm of expiry (Tur B consumes it; Tur A locks the
/// mechanism): the entity is NOT despawned, the slot stays held, and the
/// expiry lands in the AI bucket exactly once — the behavioral shape of
/// `bot_fed`.
#[tokio::test]
async fn grace_expiry_ai_handover_keeps_entity_bot_fed_stub() {
    let (ops_tx, _ops) = mpsc::channel(64);
    let mut logic = ParkLogic::new(ops_tx);
    logic.policy.insert(
        PlayerId(1),
        Detach::Hold {
            grace: Some(Duration::from_millis(80)),
            to: ExpireTo::AiHandover,
        },
    );
    let mut h = RoomH::new(park_config(RoomId(5), Some(1)), logic);

    let (entity, _a, mut out_rx) = h.join(ConnectionId(1)).await;
    h.detach(ConnectionId(1), entity, "ana").await;
    tokio::time::sleep(Duration::from_millis(220)).await;
    h.steps(3).await;

    // Still there, several ticks past the grace: handover kept it alive.
    assert_eq!(
        RoomH::entities(&mut out_rx).await,
        vec![entity],
        "AI handover preserves the entity (and its wire id)"
    );
    let s = h.latest_sample().await;
    assert_eq!(s.detach_expired_ai, 1, "recorded in the AI bucket");
    assert_eq!(s.detach_expired_despawn, 0, "not the despawn arm");
    assert_eq!(s.members, 1, "the bot holds the slot");
    // The expiry fires exactly once (deadline cleared, marker latched).
    h.steps(3).await;
    let s2 = h.latest_sample().await;
    assert_eq!(s2.detach_expired_ai, 1);

    h.shutdown().await;
}

// =====================================================================
// 6 — §12.6 may_release_veto_extends_hold_until_cleared
// =====================================================================

#[tokio::test]
async fn may_release_veto_extends_hold_until_cleared() {
    let (ops_tx, _ops) = mpsc::channel(64);
    let mut logic = ParkLogic::new(ops_tx.clone());
    // Combat-held: NO deadline — only `may_release` ends it (§14.4).
    logic.policy.insert(
        PlayerId(1),
        Detach::Hold {
            grace: None,
            to: ExpireTo::Despawn,
        },
    );
    logic.release_ok = false; // the veto ("combat nearby")
    let mut h = RoomH::new(park_config(RoomId(6), None), logic);

    let (entity, _a, mut out_rx) = h.join(ConnectionId(1)).await;
    h.detach(ConnectionId(1), entity, "ana").await;
    h.steps(6).await;
    assert_eq!(
        RoomH::entities(&mut out_rx).await,
        vec![entity],
        "veto extends the hold"
    );

    // Clear the veto THROUGH THE ORDINARY INPUT PATH: a second, live
    // session sends the test-control op (a detached row pulls nothing —
    // exactly the property test 1 locked).
    let (_e2, a2, _o2) = h.join(ConnectionId(2)).await;
    a2.send(Action {
        conn: ConnectionId(2),
        player: PlayerId(2),
        op: 0xFFFF,
        payload: bytes::Bytes::new(),
    })
    .await
    .unwrap();
    h.step().await; // ingest flips release_ok…

    // …and the very next sweeps end the hold toward Despawn.
    h.steps(2).await;
    assert!(
        RoomH::entities(&mut out_rx).await.is_empty(),
        "cleared veto ends the hold"
    );
    let s = h.latest_sample().await;
    assert_eq!(s.detach_expired_despawn, 1);

    h.shutdown().await;
}

// =====================================================================
// Registry-driven harness (supervision.rs idiom): real ticker, raw
// RegistryMsg vocabulary.
// =====================================================================

const WAIT: Duration = Duration::from_secs(5);

fn reg_config(id: RoomId) -> RoomConfig {
    RoomConfig {
        id,
        tick_hz: 60.0,
        ..Default::default()
    }
}

fn start_registry(
    factory: RoomFactory<(), (), (), ()>,
) -> (Mailbox<RegistryMsg>, tokio::task::JoinHandle<()>) {
    let (tx, rx) = channel::<RegistryMsg>(4096);
    let (ticker, _ticker_task) = Ticker::spawn(60.0, 64).expect("valid tick rate");
    let (metrics_tx, _metrics_rx) = mpsc::channel::<MetricsEvent>(64);
    let handle = tokio::spawn(
        Registry::new(
            rx,
            tx.clone(),
            factory,
            ticker,
            metrics_tx,
            None,
            None,
            None,
        )
        .run(),
    );
    (tx, handle)
}

async fn create_room(tx: &Mailbox<RegistryMsg>, cfg: RoomConfig) -> Result<RoomStatus, CoreError> {
    let (reply_tx, reply_rx) = oneshot::channel();
    tx.send(RegistryMsg::CreateRoom {
        config: cfg,
        reply: reply_tx,
    })
    .await
    .expect("registry gone");
    tokio::time::timeout(WAIT, reply_rx)
        .await
        .expect("timed out")
        .expect("reply dropped")
}

async fn destroy_room(tx: &Mailbox<RegistryMsg>, id: RoomId) -> RoomStatus {
    let (reply_tx, reply_rx) = oneshot::channel();
    tx.send(RegistryMsg::DestroyRoom {
        id,
        reply: reply_tx,
    })
    .await
    .expect("registry gone");
    tokio::time::timeout(WAIT, reply_rx)
        .await
        .expect("timed out")
        .expect("reply dropped")
}

async fn status(tx: &Mailbox<RegistryMsg>, id: RoomId) -> RoomStatus {
    let (reply_tx, reply_rx) = oneshot::channel();
    tx.send(RegistryMsg::RoomStatus {
        id,
        reply: reply_tx,
    })
    .await
    .expect("registry gone");
    tokio::time::timeout(WAIT, reply_rx)
        .await
        .expect("timed out")
        .expect("reply dropped")
}

async fn open_conn(tx: &Mailbox<RegistryMsg>, conn: ConnectionId) -> mpsc::Receiver<ConnIn> {
    let (inbox_tx, inbox_rx) = mpsc::channel(16);
    tx.send(RegistryMsg::ConnOpened {
        conn,
        inbox: inbox_tx,
    })
    .await
    .expect("registry gone");
    inbox_rx
}

async fn spawn_as(
    tx: &Mailbox<RegistryMsg>,
    conn: ConnectionId,
    room: RoomId,
    identity: &str,
) -> Result<EntityId, CoreError> {
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

/// Regression lock for global join-epoch minting: join epochs used to be
/// minted PER CONNECTION (each dispatcher restarted at 1), so a parked
/// row carrying the previous session's epoch rejected the next session's
/// FIRST resume as stale — every reconnect after an identity's first
/// cost one wasted round trip (measured: 20 stale rejects in a 20-client
/// churn run). Epochs are now minted globally by the registry at
/// dispatch, so EVERY session's first resume attempt is accepted.
#[tokio::test]
async fn repeated_reconnects_are_accepted_on_first_attempt() {
    let (tx, handle) = start_registry(parking_factory());
    let room = RoomId(72);
    create_room(&tx, reg_config(room)).await.expect("create");

    let e0 = {
        let _c = open_conn(&tx, ConnectionId(1)).await;
        spawn_as(&tx, ConnectionId(1), room, "ana")
            .await
            .expect("first session joins")
    };
    close_conn(&tx, ConnectionId(1)).await;
    tokio::time::sleep(Duration::from_millis(150)).await;

    // Three further sessions; under per-connection minting every resume
    // here needed one stale-reject + retry round trip. Each MUST be
    // accepted on its FIRST attempt and carry the SAME wire id.
    let mut expected = e0;
    for conn in [2u64, 3, 4] {
        let _c = open_conn(&tx, ConnectionId(conn)).await;
        let e = spawn_as(&tx, ConnectionId(conn), room, "ana")
            .await
            .expect("resume accepted ON THE FIRST ATTEMPT (global epochs)");
        assert_eq!(e, expected, "wire id continuity across session {conn}");
        close_conn(&tx, ConnectionId(conn)).await;
        tokio::time::sleep(Duration::from_millis(150)).await;
        expected = e;
    }

    stop_registry(tx, handle).await;
}

/// A factory whose rooms hold EVERY disconnect (the parked-slot shape the
/// supersedence test needs; the room-side policy lives here, where §3 put
/// it).
fn parking_factory() -> RoomFactory<(), (), (), ()> {
    std::sync::Arc::new(|_id, _cfg| {
        let (ops_tx, _ops) = mpsc::channel(16);
        let mut logic = ParkLogic::new(ops_tx);
        logic.hold_default = true;
        BuiltRoom::Single {
            world: (),
            logic: Box::new(logic),
        }
    })
}

/// The parking factory with a grace short enough for the sweep to fire
/// inside a test (the wildcard park is otherwise effectively permanent).
fn expiring_factory(grace: Duration) -> RoomFactory<(), (), (), ()> {
    std::sync::Arc::new(move |_id, _cfg| {
        let (ops_tx, _ops) = mpsc::channel(16);
        let mut logic = ParkLogic::new(ops_tx);
        logic.hold_default = true;
        logic.hold_grace = grace;
        BuiltRoom::Single {
            world: (),
            logic: Box::new(logic),
        }
    })
}

/// [`start_registry`] keeping the metrics receiver, so a test can read the
/// registry's OWN table size (`RegistrySample::conns`) instead of
/// inferring it.
fn start_registry_observed(
    factory: RoomFactory<(), (), (), ()>,
) -> (
    Mailbox<RegistryMsg>,
    mpsc::Receiver<MetricsEvent>,
    tokio::task::JoinHandle<()>,
) {
    let (tx, rx) = channel::<RegistryMsg>(4096);
    let (ticker, _ticker_task) = Ticker::spawn(60.0, 64).expect("valid tick rate");
    let (metrics_tx, metrics_rx) = mpsc::channel::<MetricsEvent>(256);
    let handle = tokio::spawn(
        Registry::new(
            rx,
            tx.clone(),
            factory,
            ticker,
            metrics_tx,
            None,
            None,
            None,
        )
        .run(),
    );
    (tx, metrics_rx, handle)
}

/// Drain whatever the registry has emitted and return its latest sample.
/// The registry flushes on state change, so the caller triggers one first.
async fn latest_registry_sample(
    metrics: &mut mpsc::Receiver<MetricsEvent>,
) -> gsb_core::metrics::RegistrySample {
    let mut last = None;
    while let Ok(ev) = metrics.try_recv() {
        if let MetricsEvent::Registry(s) = ev {
            last = Some(s);
        }
    }
    last.expect("registry emitted at least one sample")
}

/// A park that ends in DESPAWN must release the REGISTRY's table row, not
/// only the room's.
///
/// The room owns the park deadline (the grace is room-side policy), so the
/// room is the only actor that learns the hold ended — and it used to tell
/// nobody. The registry's `detached` row therefore outlived the entity it
/// was holding a slot for, and the only things that could ever release it
/// were a resume for the same identity or the room ending. Neither happens
/// to a player who simply never comes back to a persistent room, so every
/// abandoned session leaked one row AND one `max_connections` slot,
/// permanently: the cap eventually refuses live players on behalf of
/// sessions that ended long ago.
///
/// Read through `RegistrySample::conns` — the leaked table itself, not a
/// proxy for it.
#[tokio::test]
async fn park_expiry_releases_the_registry_row() {
    let (tx, mut metrics, handle) =
        start_registry_observed(expiring_factory(Duration::from_millis(80)));
    let room = RoomId(73);
    create_room(&tx, reg_config(room)).await.expect("create");

    let _c1 = open_conn(&tx, ConnectionId(1)).await;
    spawn_as(&tx, ConnectionId(1), room, "ana")
        .await
        .expect("first session joins");
    // Transport death → the room parks the entity and holds the slot.
    close_conn(&tx, ConnectionId(1)).await;

    // Outlive the grace so the room's sweep expires the hold and despawns
    // through the ordinary leave funnel.
    tokio::time::sleep(Duration::from_millis(400)).await;

    // A second connection, opened only to make the registry flush a fresh
    // sample (its counters emit on state change). With the expired park
    // released, the table holds exactly this one connection.
    let _c2 = open_conn(&tx, ConnectionId(2)).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    let s = latest_registry_sample(&mut metrics).await;
    assert_eq!(
        s.conns, 1,
        "the expired park's row must be released; only the live conn #2 \
         should remain in the registry table"
    );

    stop_registry(tx, handle).await;
}

/// The sharded twin of [`expiring_factory`]: two shards of the same park
/// logic, every join homing to shard 0.
fn expiring_sharded_factory(grace: Duration) -> RoomFactory<(), (), (), ()> {
    std::sync::Arc::new(move |_id, _cfg| {
        let shard = |index: usize| {
            let (ops_tx, _ops) = mpsc::channel(16);
            let mut logic = ParkLogic::new(ops_tx);
            logic.hold_default = true;
            logic.hold_grace = grace;
            logic.index = index;
            (
                (),
                Box::new(logic) as Box<dyn ShardLogic<(), GroupKey = (), State = (), Strip = ()>>,
            )
        };
        BuiltRoom::Sharded {
            shards: vec![shard(0), shard(1)],
            home_shard: std::sync::Arc::new(|_conn, _identity: &str| 0),
        }
    })
}

/// The shard actor carries its own copy of the park sweep, so it carries
/// its own copy of the leak: [`park_expiry_releases_the_registry_row`] for
/// the grid.
///
/// The sharded path has a second thing to get right — a detached row also
/// holds a slot in the registry's `ShardGroup` member count (which is what
/// enforces `max_players` for a sharded room, since no single actor sees
/// the whole roster). Releasing the row must hand that count back too, or
/// the room stays "full" forever with nobody in it.
#[tokio::test]
async fn sharded_park_expiry_releases_the_registry_row_and_the_member_slot() {
    // The grace has to outlive the settle window below (the assertion
    // that the hold is still ALIVE), so it is longer than the single-room
    // test's.
    let (tx, mut metrics, handle) =
        start_registry_observed(expiring_sharded_factory(Duration::from_millis(500)));
    let room = RoomId(74);
    create_room(&tx, reg_config(room)).await.expect("create");

    let _c1 = open_conn(&tx, ConnectionId(1)).await;
    spawn_as(&tx, ConnectionId(1), room, "ana")
        .await
        .expect("first session joins");
    close_conn(&tx, ConnectionId(1)).await;
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(
        status(&tx, room).await,
        RoomStatus::Running { members: 1 },
        "the parked player holds its slot while the hold is alive"
    );

    // Outlive the grace: the shard's sweep expires the hold and despawns.
    tokio::time::sleep(Duration::from_millis(700)).await;

    assert_eq!(
        status(&tx, room).await,
        RoomStatus::Running { members: 0 },
        "the expired park must hand its member slot back"
    );

    let _c2 = open_conn(&tx, ConnectionId(2)).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    let s = latest_registry_sample(&mut metrics).await;
    assert_eq!(
        s.conns, 1,
        "the expired park's row must be released; only the live conn #2 \
         should remain in the registry table"
    );

    stop_registry(tx, handle).await;
}

/// A factory whose rooms DECLINE to park: `on_disconnect` answers
/// [`Detach::Despawn`] straight away. This is the shipped shape of
/// `disconnect_grace_secs = 0` (`gsb-game`'s `park_on_disconnect` returns
/// `Despawn` on a zero grace), and of every policy that decides this
/// particular player is not worth holding.
fn declining_factory() -> RoomFactory<(), (), (), ()> {
    std::sync::Arc::new(|_id, _cfg| {
        let (ops_tx, _ops) = mpsc::channel(16);
        // `hold_default = false` + no per-conn policy entry = the
        // `unwrap_or(Detach::Despawn)` arm of `ParkLogic::on_disconnect`.
        let logic = ParkLogic::new(ops_tx);
        BuiltRoom::Single {
            world: (),
            logic: Box::new(logic),
        }
    })
}

/// A policy that declines to park leaks the registry row exactly the way
/// an unreported hold expiry did.
///
/// The registry marks a closing connection's row `detached` and KEEPS it
/// (§4 — the slot is held for the park) BEFORE the room's policy has
/// answered; it then waits to be told how the detach ended. The hold-
/// expiry sweep tells it. The `Detach::Despawn` arm — where the policy
/// declines and the room despawns immediately — used to tell it nothing,
/// and no sweep ever runs for a park that never started: there is no hold
/// and no deadline. So the row stood forever with `room = Some(..)` and
/// `detached = true`, holding a `max_connections` slot and counting
/// toward `room_members` for an entity that was already gone.
///
/// This is the DEFAULT-OFF configuration's path: with
/// `disconnect_grace_secs = 0` every single disconnect leaked.
#[tokio::test]
async fn declined_park_releases_the_registry_row() {
    let (tx, mut metrics, handle) = start_registry_observed(declining_factory());
    let room = RoomId(75);
    create_room(&tx, reg_config(room)).await.expect("create");

    let _c1 = open_conn(&tx, ConnectionId(1)).await;
    spawn_as(&tx, ConnectionId(1), room, "ana")
        .await
        .expect("first session joins");
    // Transport death → the policy declines to park → the room despawns
    // in the very same control phase. Nothing is left to expire.
    close_conn(&tx, ConnectionId(1)).await;
    tokio::time::sleep(Duration::from_millis(300)).await;

    assert_eq!(
        status(&tx, room).await,
        RoomStatus::Running { members: 0 },
        "a declined park holds no slot: the entity was despawned on the \
         spot, so the registry's member view must not still count it"
    );

    // A second connection, opened only to make the registry flush a fresh
    // sample (its counters emit on state change). With the declined
    // park's row released, the table holds exactly this one connection.
    let _c2 = open_conn(&tx, ConnectionId(2)).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    let s = latest_registry_sample(&mut metrics).await;
    assert_eq!(
        s.conns, 1,
        "the declined park's row must be released; only the live conn #2 \
         should remain in the registry table"
    );

    stop_registry(tx, handle).await;
}

/// The sharded twin of [`declining_factory`]: two shards of the same park
/// logic, every join homing to shard 0, none of them parking.
fn declining_sharded_factory() -> RoomFactory<(), (), (), ()> {
    std::sync::Arc::new(move |_id, _cfg| {
        let shard = |index: usize| {
            let (ops_tx, _ops) = mpsc::channel(16);
            let mut logic = ParkLogic::new(ops_tx);
            logic.index = index;
            (
                (),
                Box::new(logic) as Box<dyn ShardLogic<(), GroupKey = (), State = (), Strip = ()>>,
            )
        };
        BuiltRoom::Sharded {
            shards: vec![shard(0), shard(1)],
            home_shard: std::sync::Arc::new(|_conn, _identity: &str| 0),
        }
    })
}

/// The shard actor runs its own copy of the detach policy
/// (`ShardMsg::Detach`), so it carries its own copy of the declined-park
/// leak: [`declined_park_releases_the_registry_row`] for the grid.
///
/// As on the hold-expiry path, the sharded case has the extra thing to
/// get right — a detached row also holds a slot in the registry's
/// `ShardGroup` member count, which is what enforces `max_players` for a
/// sharded room (no single shard sees the whole roster). A declined park
/// that reports nothing leaves the room permanently "full" with nobody
/// in it.
#[tokio::test]
async fn sharded_declined_park_releases_the_registry_row_and_the_member_slot() {
    let (tx, mut metrics, handle) = start_registry_observed(declining_sharded_factory());
    let room = RoomId(76);
    create_room(&tx, reg_config(room)).await.expect("create");

    let _c1 = open_conn(&tx, ConnectionId(1)).await;
    spawn_as(&tx, ConnectionId(1), room, "ana")
        .await
        .expect("first session joins");
    close_conn(&tx, ConnectionId(1)).await;
    tokio::time::sleep(Duration::from_millis(300)).await;

    assert_eq!(
        status(&tx, room).await,
        RoomStatus::Running { members: 0 },
        "the declined park must hand its member slot back"
    );

    let _c2 = open_conn(&tx, ConnectionId(2)).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    let s = latest_registry_sample(&mut metrics).await;
    assert_eq!(
        s.conns, 1,
        "the declined park's row must be released; only the live conn #2 \
         should remain in the registry table"
    );

    stop_registry(tx, handle).await;
}

async fn stop_registry(tx: Mailbox<RegistryMsg>, handle: tokio::task::JoinHandle<()>) {
    tx.send(RegistryMsg::Shutdown).await.ok();
    drop(tx);
    handle.await.ok();
}

// =====================================================================
// 7 — §12.7 double_session_supersedes_the_parked_one
// =====================================================================

#[tokio::test]
async fn double_session_supersedes_the_parked_one() {
    let (tx, handle) = start_registry(parking_factory());
    let room = RoomId(71);
    create_room(&tx, reg_config(room)).await.expect("create");

    // c1 ("ana") joins, then its transport dies → the affiliation parks.
    let _c1_inbox = open_conn(&tx, ConnectionId(1)).await;
    let e1 = spawn_as(&tx, ConnectionId(1), room, "ana")
        .await
        .expect("first session joins");
    close_conn(&tx, ConnectionId(1)).await;
    // Detach travels dispatcher → room → back as DetachDone: settle.
    tokio::time::sleep(Duration::from_millis(150)).await;

    // The parked identity still counts (slot held, §4).
    assert_eq!(
        status(&tx, room).await,
        RoomStatus::Running { members: 1 },
        "parked player holds the slot"
    );

    // c9 ("ana") returns: the implicit resume re-affiliates the NEW conn
    // and releases the old detached entry — members stay at 1.
    let e9 = spawn_as(&tx, ConnectionId(9), room, "ana")
        .await
        .expect("resume accepted");
    assert_eq!(e9, e1, "same entity across sessions");
    assert_eq!(
        status(&tx, room).await,
        RoomStatus::Running { members: 1 },
        "old detached entry released, not stacked"
    );

    // Double-session with BOTH sockets live: the newer wins; the older
    // socket gets ERROR 9 (`ConnIn::ServerClosed`) and loses the seat.
    let mut bob_inbox = open_conn(&tx, ConnectionId(30)).await;
    spawn_as(&tx, ConnectionId(30), room, "bob")
        .await
        .expect("bob joins");
    let _bob2_inbox = open_conn(&tx, ConnectionId(31)).await;
    spawn_as(&tx, ConnectionId(31), room, "bob")
        .await
        .expect("bob #2 supersedes");
    match tokio::time::timeout(WAIT, bob_inbox.recv()).await {
        Ok(Some(ConnIn::ServerClosed { cause, reason })) => {
            assert_eq!(cause, gsb_core::conn::ServerClose::Superseded);
            assert!(!reason.is_empty())
        }
        other => panic!("expected ServerClosed for the superseded socket: {other:?}"),
    }
    assert_eq!(
        status(&tx, room).await,
        RoomStatus::Running { members: 2 },
        "ana(resumed) + bob#2 only"
    );

    stop_registry(tx, handle).await;
}

async fn close_conn(tx: &Mailbox<RegistryMsg>, conn: ConnectionId) {
    tx.send(RegistryMsg::ConnClosed { conn })
        .await
        .expect("registry gone");
}

// =====================================================================
// 8 — §12.8 room classes
// =====================================================================

#[tokio::test]
async fn connect_to_retired_ephemeral_room_is_error_12() {
    // An anonymous room: nobody parks, nothing resumes — pure lifecycle.
    let (tx, handle) = start_registry(std::sync::Arc::new(|_id, _cfg| BuiltRoom::Single {
        world: (),
        logic: Box::new(ParkLogic::new(mpsc::channel(16).0)),
    }));
    let room = RoomId(81);
    create_room(&tx, reg_config(room)).await.expect("create");

    // The match ends (operator destroy): the id retires.
    destroy_room(&tx, room).await;

    // Connect/resume against the retired id → ERROR 12 semantics.
    let _inbox = open_conn(&tx, ConnectionId(1)).await;
    match spawn_as(&tx, ConnectionId(1), room, "ana").await {
        Err(CoreError::RoomRetired(id)) => assert_eq!(id, room.0),
        other => panic!("expected RoomRetired, got {other:?}"),
    }
    // An EXPLICIT re-create is the operator revisiting the decision (§8
    // protects against AUTOMATIC resurrection): it un-retires the id and
    // serves normally again.
    let st = create_room(&tx, reg_config(room))
        .await
        .expect("explicit re-create overrides retirement");
    assert!(matches!(st, RoomStatus::Running { members: 0 }));
    spawn_as(&tx, ConnectionId(3), room, "rey")
        .await
        .expect("un-retired room serves joins");
    destroy_room(&tx, room).await;

    // Contrast: a NEVER-known room answers the pre-existing code (4).
    match spawn_as(&tx, ConnectionId(2), RoomId(82), "x").await {
        Err(CoreError::RoomNotFound(_)) => {}
        other => panic!("expected RoomNotFound for unknown room, got {other:?}"),
    }

    stop_registry(tx, handle).await;
}

struct PanicAfterJoin {
    joined: bool,
    armed: bool,
}

impl GameLogic<()> for PanicAfterJoin {
    type GroupKey = ();
    type Strip = ();
    fn snapshot_op(&self) -> u16 {
        0x7600
    }
    fn private_op(&self) -> u16 {
        0x7601
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
        false
    }
    fn on_join(&mut self, _w: &mut (), c: ConnectionId) -> Admission {
        self.joined = true;
        Admission {
            player: PlayerId(c.0),
            entity: 1,
        }
    }
    fn on_leave(&mut self, _w: &mut (), _p: PlayerId) {}
    fn ingest(&mut self, _w: &mut (), _c: &TickCtx, a: &mut Vec<Action>) {
        a.clear();
    }
    fn update(&mut self, _w: &mut (), _c: &TickCtx) {
        if self.joined && self.armed {
            panic!("reconnect test: persistent room panics once");
        }
    }
}

impl RoomLogic<()> for PanicAfterJoin {}

#[tokio::test]
async fn persistent_room_rebuilds_after_panic_even_without_flag() {
    let build_n = std::sync::atomic::AtomicU64::new(0);
    let factory: RoomFactory<(), (), (), ()> = std::sync::Arc::new(move |_id, _cfg| {
        let n = build_n.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
        BuiltRoom::Single {
            world: (),
            logic: Box::new(PanicAfterJoin {
                joined: false,
                armed: n == 1,
            }),
        }
    });
    let (tx, handle) = start_registry(factory);
    let room = RoomId(83);
    // restart_on_panic = false ON PURPOSE: persistence forces the rebuild.
    let cfg = RoomConfig {
        persistent: true,
        ..reg_config(room)
    };
    create_room(&tx, cfg).await.expect("create");

    let _inbox = open_conn(&tx, ConnectionId(1)).await;
    spawn_as(&tx, ConnectionId(1), room, "ana")
        .await
        .expect("joined the doomed incarnation");

    // Poll until the class guarantee restores service (death report →
    // reap → forced rebuild — asynchronous by design).
    let deadline = Instant::now() + WAIT;
    loop {
        match status(&tx, room).await {
            RoomStatus::Running { members: 0 } => break,
            s => {
                assert!(
                    Instant::now() < deadline,
                    "persistent room never rebuilt: {s:?}"
                );
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }
    }
    // The rebuild came back EMPTY and serves fresh joins.
    let _inbox2 = open_conn(&tx, ConnectionId(2)).await;
    spawn_as(&tx, ConnectionId(2), room, "bea")
        .await
        .expect("rebuilt room serves joins");

    stop_registry(tx, handle).await;
}

// =====================================================================
// 3 — §12.3 broadcast_resume_accepted_by_exactly_one_shard
// =====================================================================

mod shard_test {
    //! Two-shard harness for the §6 single-winner guarantee. Minimal
    //! ShardLogic: a map-shaped world, a one-entry park ledger, no
    //! neighbor traffic.

    use super::*;

    #[derive(Default)]
    pub struct SWorld {
        pub ents: HashMap<u64, PlayerId>,
    }

    enum SLedg {
        Held(PlayerId),
    }

    pub struct SLogic {
        index: usize,
        serial: u64,
        player_ent: HashMap<PlayerId, u64>,
        hold_on_disconnect: bool,
        ledger: HashMap<String, SLedg>,
    }

    impl GameLogic<SWorld> for SLogic {
        type GroupKey = ();
        type Strip = ();
        fn snapshot_op(&self) -> u16 {
            0x7700
        }
        fn private_op(&self) -> u16 {
            0x7701
        }
        fn group_of(&self, _w: &SWorld, _p: PlayerId) -> Self::GroupKey {}
        fn snapshot(
            &mut self,
            _w: &mut SWorld,
            _c: &TickCtx,
            _g: &(),
            _borrowed: &[BorderRecord<()>],
            _o: &mut bytes::BytesMut,
        ) -> bool {
            false
        }
        fn on_join(&mut self, w: &mut SWorld, conn: ConnectionId) -> Admission {
            self.serial += 1;
            let wire = self.index as u64 * 1000 + self.serial;
            // Test identity policy: the conn id doubles as the player id —
            // which makes "same player after resume/migration" directly
            // observable as a stable key.
            let player = PlayerId(conn.0);
            w.ents.insert(wire, player);
            self.player_ent.insert(player, wire);
            Admission {
                player,
                entity: wire,
            }
        }
        fn on_leave(&mut self, w: &mut SWorld, player: PlayerId) {
            if let Some(wire) = self.player_ent.remove(&player) {
                w.ents.remove(&wire);
            }
        }
        fn ingest(&mut self, _w: &mut SWorld, _c: &TickCtx, a: &mut Vec<Action>) {
            a.clear();
        }
        fn update(&mut self, _w: &mut SWorld, _c: &TickCtx) {}

        fn on_disconnect(&mut self, _w: &mut SWorld, player: PlayerId, identity: &str) -> Detach {
            if self.hold_on_disconnect && self.player_ent.contains_key(&player) {
                self.ledger
                    .insert(identity.to_string(), SLedg::Held(player));
                return Detach::Hold {
                    grace: Some(Duration::from_secs(3600)),
                    to: ExpireTo::Despawn,
                };
            }
            Detach::Despawn
        }
        fn resume_lookup(&self, _w: &SWorld, identity: &str) -> ResumeFound {
            match self.ledger.get(identity) {
                Some(SLedg::Held(p)) => ResumeFound::Held(*p),
                None => ResumeFound::Never,
            }
        }
        fn on_resume(
            &mut self,
            _w: &mut SWorld,
            identity: &str,
            _conn: ConnectionId,
            _player: PlayerId,
            _entity: EntityId,
        ) {
            // Only the ledger entry is consumed; the player-keyed tables
            // keep their keys across the resume (Faz 2).
            self.ledger.remove(identity);
        }
    }

    // Faz 1 trait split: the sharding seam stays on `ShardLogic`.
    impl ShardLogic<SWorld> for SLogic {
        type State = ();

        fn index(&self) -> usize {
            self.index
        }
        fn shard_count(&self) -> usize {
            2
        }
        fn serial_capacity(&self) -> u64 {
            1000
        }
        fn serial_used(&self) -> u64 {
            self.serial
        }
        fn neighbors(&self) -> &[usize] {
            &[]
        }
        fn collect_migrations(&mut self, _w: &mut SWorld, _nb: usize) -> Vec<Migrating<()>> {
            Vec::new()
        }
        fn on_migrate_in(
            &mut self,
            _w: &mut SWorld,
            _wire: u64,
            _state: (),
            _player: Option<PlayerId>,
        ) {
        }
        fn on_migrate_out(&mut self, _w: &mut SWorld, _wire: u64) {}
        fn collect_border(&self, _w: &SWorld) -> Vec<BorderRecord<()>> {
            Vec::new()
        }
        fn own_wires(&self, w: &SWorld) -> Vec<u64> {
            w.ents.keys().copied().collect()
        }
    }

    pub struct ShardPair {
        pub tick_tx: tokio::sync::broadcast::Sender<TickInfo>,
        pub shards: [Mailbox<gsb_core::shard::ShardMsg<(), ()>>; 2],
        pub handles: Vec<tokio::task::JoinHandle<()>>,
        t0: Instant,
        next_tick: u64,
    }

    impl ShardPair {
        pub fn new(hold: bool) -> Self {
            let (tick_tx, _) = tokio::sync::broadcast::channel(64);
            let (tx0, rx0) = channel::<gsb_core::shard::ShardMsg<(), ()>>(128);
            let (tx1, rx1) = channel::<gsb_core::shard::ShardMsg<(), ()>>(128);
            let (dummy, _d) = channel::<gsb_core::shard::ShardMsg<(), ()>>(1);
            let mk_cfg = || RoomConfig {
                id: RoomId(91),
                tick_hz: 60.0,
                keepalive_hz: 0.0,
                metrics_cadence_hz: 0.0,
                ..Default::default()
            };
            let (m0, _r0) = mpsc::channel(1);
            let (m1, _r1) = mpsc::channel(1);
            let mk_logic = |index: usize| SLogic {
                index,
                serial: 0,
                player_ent: HashMap::new(),
                hold_on_disconnect: hold,
                ledger: HashMap::new(),
            };
            let h0 = tokio::spawn(
                ShardActor::new(
                    mk_cfg(),
                    0,
                    SWorld::default(),
                    Box::new(mk_logic(0)),
                    tick_tx.subscribe(),
                    rx0,
                    vec![dummy.clone(), tx1.clone()],
                    1,
                    m0,
                    None, // no result sink in the reconnect harness
                )
                .run(),
            );
            let h1 = tokio::spawn(
                ShardActor::new(
                    mk_cfg(),
                    1,
                    SWorld::default(),
                    Box::new(mk_logic(1)),
                    tick_tx.subscribe(),
                    rx1,
                    vec![tx0.clone(), dummy],
                    1,
                    m1,
                    None, // no result sink in the reconnect harness
                )
                .run(),
            );
            Self {
                tick_tx,
                shards: [tx0, tx1],
                handles: vec![h0, h1],
                t0: Instant::now(),
                next_tick: 0,
            }
        }

        pub async fn tick(&mut self) {
            self.next_tick += 1;
            let at = self.t0 + Duration::from_secs_f64(self.next_tick as f64 / 60.0);
            self.tick_tx
                .send(TickInfo {
                    tick: self.next_tick,
                    at,
                })
                .expect("subscribed");
            tokio::time::sleep(Duration::from_millis(25)).await;
        }

        pub async fn join(&mut self, shard: usize, conn: ConnectionId, epoch: u64) -> EntityId {
            let (out, _o) = mpsc::channel(8);
            let (reply_tx, reply_rx) = oneshot::channel();
            self.shards[shard]
                .send(gsb_core::shard::ShardMsg::Join {
                    conn,
                    epoch,
                    identity: String::new(),
                    out,
                    reply: reply_tx,
                })
                .await
                .expect("shard alive");
            self.tick().await;
            reply_rx.await.expect("join reply").expect("join ok").0
        }

        /// The registry's broadcast shape: the SAME detach goes to BOTH
        /// shards; only the owner matches the entity guard.
        pub async fn detach_broadcast(
            &mut self,
            conn: ConnectionId,
            entity: EntityId,
            identity: &str,
        ) {
            for s in &self.shards {
                s.send(gsb_core::shard::ShardMsg::Detach {
                    conn,
                    entity,
                    identity: identity.to_string(),
                })
                .await
                .expect("shard alive");
            }
            self.tick().await;
        }

        /// Broadcast one resume to BOTH shards; returns the accepted wire
        /// id per shard (`None` = "not here").
        pub async fn resume_broadcast(
            &mut self,
            conn: ConnectionId,
            epoch: u64,
            identity: &str,
        ) -> [Option<EntityId>; 2] {
            let mut rxs = Vec::new();
            for s in &self.shards {
                let (out, _o) = mpsc::channel(8);
                let (reply_tx, reply_rx) = oneshot::channel();
                s.send(gsb_core::shard::ShardMsg::Resume {
                    conn,
                    epoch,
                    identity: identity.to_string(),
                    out,
                    reply: reply_tx,
                })
                .await
                .expect("shard alive");
                rxs.push(reply_rx);
            }
            self.tick().await;
            let mut out = [None, None];
            for (i, rx) in rxs.into_iter().enumerate() {
                out[i] = match tokio::time::timeout(Duration::from_secs(5), rx).await {
                    Ok(Ok(Ok(Some((e, _))))) => Some(e),
                    _ => None,
                };
            }
            out
        }
    }

    pub async fn kill(handles: Vec<tokio::task::JoinHandle<()>>) {
        for h in handles {
            h.abort();
        }
    }
}

#[tokio::test]
async fn broadcast_resume_accepted_by_exactly_one_shard() {
    let mut p = shard_test::ShardPair::new(true);
    // The player homes to shard 0.
    let wire = p.join(0, ConnectionId(1), 1).await;
    // Transport dies; the DETACH broadcasts, shard 0 parks.
    p.detach_broadcast(ConnectionId(1), wire, "ana").await;

    // One resume broadcast: BOTH shards see it; EXACTLY ONE accepts, and
    // it answers with the SAME wire id (single-winner, §6).
    let [r0, r1] = p.resume_broadcast(ConnectionId(9), 2, "ana").await;
    assert_eq!(
        [r0, r1],
        [Some(wire), None],
        "exactly one shard accepts; the other answers 'not here'"
    );

    // A second resume of the consumed park finds nothing anywhere: no
    // resurrection, no double accept.
    let [r0b, r1b] = p.resume_broadcast(ConnectionId(10), 3, "ana").await;
    assert_eq!([r0b, r1b], [None, None]);

    shard_test::kill(p.handles).await;
}

// =====================================================================
// Extra — pending-RPC rebind: a late worker report addressed to the OLD
// connection finds nothing, is counted, and hurts nothing (§11 edge row +
// the RebindKey signposts).
// =====================================================================

#[tokio::test]
async fn resume_with_pending_rpc_late_report_is_harmless() {
    let (ops_tx, _ops) = mpsc::channel(64);
    let (slots_tx, mut slots_rx) =
        mpsc::channel::<oneshot::Sender<Result<bytes::Bytes, String>>>(4);
    let mut logic = ParkLogic::new(ops_tx);
    logic.policy.insert(PlayerId(1), hold_forever());
    logic.req_slots = slots_tx;
    let mut h = RoomH::new(
        RoomConfig {
            request_timeout: Duration::from_secs(60),
            ..park_config(RoomId(41), None)
        },
        logic,
    );

    let (entity, actions, _out) = h.join(ConnectionId(1)).await;

    // Fire an external request: base envelope RPC_REQ, inner op 0x2001.
    let env = gsb_protocol::base::RpcRequest {
        id: 77,
        op: 0x2001,
        payload: vec![],
    };
    actions
        .send(Action {
            conn: ConnectionId(1),
            player: PlayerId(1),
            op: gsb_protocol::op::base::RPC_REQ,
            payload: env.encode_to_vec().into(),
        })
        .await
        .unwrap();
    h.step().await; // registered pending; worker awaits our oneshot
    let slot = tokio::time::timeout(Duration::from_secs(5), slots_rx.recv())
        .await
        .expect("no request slot handed out")
        .expect("slot channel closed");

    // The transport dies mid-flight (pending drops with it, §11), then the
    // identity resumes onto a fresh socket — RebindKey ran over every
    // table.
    h.detach(ConnectionId(1), entity, "ana").await;
    let (entity2, _a2) = h.resume(ConnectionId(9), 1, "ana").await.expect("resume");
    assert_eq!(entity2, entity);

    // NOW the ancient dependency resolves: the report is addressed to the
    // OLD conn. It must find no pending entry, be COUNTED, hurt nothing.
    slot.send(Ok(bytes::Bytes::from_static(b"late"))).ok();
    tokio::time::sleep(Duration::from_millis(120)).await; // worker reports
    h.steps(3).await;

    let s = h.latest_sample().await;
    assert_eq!(s.requests_late, 1, "the late report is reconciled away");
    assert_eq!(s.resumes, 1, "the resume stands");

    h.shutdown().await;
}
