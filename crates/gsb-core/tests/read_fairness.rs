//! READ-phase reach fairness (rotation) under sustained overload.
//!
//! The room-level pull budget ([`RoomConfig::max_pending_actions`]) bounds
//! how many actions one tick can ingest. When that budget cannot cover
//! every connection's pending input, WHO gets served is decided by the
//! scan order. A fixed order (the old `conns` `HashMap` walk) lets the
//! same hash-order prefix consume the whole budget on every tick, so the
//! tail connections are never REACHED: their actions stay deferred
//! forever (the room drops nothing, but "deferred" must not mean "never").
//! The rotating cursor over the join-order roster guarantees the opposite
//! property, pinned here:
//!
//! under a sustained overload, EVERY connection has at least one action
//! ingested within `roster.len()` ticks, no matter how much a persistent
//! flooder ahead of it holds.
//!
//! The room actor is driven directly (manual broadcast feed — the same
//! idiom as `rpc.rs`): the test owns the tick sender, the control channel,
//! and every connection's action mailbox. Per-tick progress is observed
//! through the metrics channel (one sample per step at a 30 Hz cadence),
//! and the per-connection first-ingest ledger rides the match-result seam
//! out of the actor at shutdown.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::time::{Duration, Instant};

use gsb_core::channel::{channel, FrameBatch, Mailbox};
use gsb_core::id::RoomId;
use gsb_core::metrics::{MetricsEvent, RoomSample};
use gsb_core::registry::MatchResult;
use gsb_core::room::{Action, GameLogic, RoomActor, RoomConfig, RoomControl, RoomLogic, TickCtx};
use gsb_core::ticker::TickInfo;
use tokio::sync::{broadcast, mpsc, oneshot};

const OP_SNAP: u16 = 0x2001;
const OP_PRIV: u16 = 0x2002;

/// World = the first global tick index at which each connection had an
/// action INGESTED (`or_insert`: later actions never overwrite it). Small
/// enough to ship whole through the match-result seam.
type FirstSeen = BTreeMap<u64, u64>;

/// A logic that records first-ingest ticks and reports them on shutdown.
struct RecorderLogic;

impl GameLogic<FirstSeen> for RecorderLogic {
    type GroupKey = ();
    type Strip = ();

    fn snapshot_op(&self) -> u16 {
        OP_SNAP
    }

    fn private_op(&self) -> u16 {
        OP_PRIV
    }

    fn group_of(&self, _w: &FirstSeen, _p: gsb_core::PlayerId) -> Self::GroupKey {}

    fn snapshot(
        &mut self,
        _w: &mut FirstSeen,
        _c: &TickCtx,
        _g: &(),
        _borrowed: &[gsb_core::shard::BorderRecord<()>],
        _o: &mut bytes::BytesMut,
    ) -> bool {
        false // no snapshots: the observation seam is the match result
    }

    fn on_join(&mut self, _w: &mut FirstSeen, c: gsb_core::ConnectionId) -> gsb_core::room::Admission {
        // Test identity policy: the conn id doubles as the player id.
        gsb_core::room::Admission {
            player: gsb_core::PlayerId(c.0),
            entity: 1,
        }
    }

    fn on_leave(&mut self, _w: &mut FirstSeen, _p: gsb_core::PlayerId) {}

    fn ingest(&mut self, w: &mut FirstSeen, ctx: &TickCtx, actions: &mut Vec<Action>) {
        for a in actions.iter() {
            w.entry(a.player.0).or_insert(ctx.tick);
        }
    }

    fn update(&mut self, _w: &mut FirstSeen, _c: &TickCtx) {}

    // Faz 3: the result seam lives on the shared `GameLogic` supertrait.
    /// `[n: u32 LE]` then n × `[conn: u64 LE][first_tick: u64 LE]`.
    fn match_result(&mut self, w: &mut FirstSeen) -> Option<bytes::Bytes> {
        let mut buf = bytes::BytesMut::new();
        buf.extend_from_slice(&(w.len() as u32).to_le_bytes());
        for (conn, tick) in w.iter() {
            buf.extend_from_slice(&conn.to_le_bytes());
            buf.extend_from_slice(&tick.to_le_bytes());
        }
        Some(buf.freeze())
    }
}

// Faz 3 promotion: `match_result` moved to `GameLogic`; this impl stays
// as the single-room marker.
impl RoomLogic<FirstSeen> for RecorderLogic {}

struct Harness {
    tick_tx: broadcast::Sender<TickInfo>,
    control: Mailbox<RoomControl>,
    metrics_rx: mpsc::Receiver<MetricsEvent>,
    results: mpsc::Receiver<MatchResult>,
    handle: tokio::task::JoinHandle<()>,
    t0: Instant,
    next_tick: u64,
    actions: HashMap<gsb_core::ConnectionId, Mailbox<Action>>,
}

impl Harness {
    async fn new(config: RoomConfig) -> Self {
        let (tick_tx, _first) = broadcast::channel(64);
        let tick_rx = tick_tx.subscribe();
        let (control, control_rx) = channel(config.control_capacity);
        // Every step emits one sample (cadence == tick rate below); the
        // buffer holds the whole test so no sample is ever dropped and a
        // recv is a reliable per-step barrier.
        let (metrics_tx, metrics_rx) = mpsc::channel::<MetricsEvent>(512);
        let (results_tx, results) = mpsc::channel::<MatchResult>(4);
        let actor = RoomActor::new(
            config,
            FirstSeen::default(),
            Box::new(RecorderLogic),
            tick_rx,
            control_rx,
            1, // room rate == global rate: every tick is a step
            metrics_tx,
            Some(results_tx),
        );
        Self {
            tick_tx,
            control,
            metrics_rx,
            results,
            handle: tokio::spawn(actor.run()),
            t0: Instant::now(),
            next_tick: 0,
            actions: HashMap::new(),
        }
    }

    fn tick(&mut self) {
        self.next_tick += 1;
        let at = self.t0 + Duration::from_secs_f64(self.next_tick as f64 / 30.0);
        self.tick_tx
            .send(TickInfo {
                tick: self.next_tick,
                at,
            })
            .expect("room subscriber alive");
    }

    /// One completed step, observed through its end-of-step metrics
    /// sample: the step body is synchronous and the metrics send is its
    /// last act, so receiving a sample proves a full step ran.
    async fn step_done(&mut self) -> RoomSample {
        match tokio::time::timeout(Duration::from_secs(2), self.metrics_rx.recv())
            .await
            .expect("timed out waiting for the step's metrics sample")
            .expect("metrics channel closed")
        {
            MetricsEvent::Room(s) => s,
            other => panic!("unexpected metrics event {other:?}"),
        }
    }

    /// Consume exactly one sample per step sent so far. This is the
    /// synchronization primitive of the harness: the room emits exactly
    /// one sample per step (cadence == tick rate), so once the backlog is
    /// drained to zero, every subsequent recv is the fresh sample of a
    /// step that completed after this point — without it, a recv could
    /// satisfy itself from stale join-phase samples and the test would
    /// race ahead of the room (a real bug this barrier model caught).
    async fn sync_steps(&mut self) {
        for _ in 0..self.next_tick {
            self.step_done().await;
        }
    }

    async fn join(&mut self, conn: gsb_core::ConnectionId) {
        let (out_tx, _out_rx) = mpsc::channel::<FrameBatch>(8);
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
        // Two ticks: the join is processed on the first; the second keeps
        // the cadence of the other harnesses (and guarantees membership
        // before the test depends on it).
        self.tick();
        self.tick();
        let (_entity, actions) = tokio::time::timeout(Duration::from_secs(2), reply_rx)
            .await
            .expect("timed out waiting for the join reply")
            .expect("join reply dropped")
            .expect("join accepted (room not full)");
        self.actions.insert(conn, actions);
    }

    /// Stop the room and wait for its task to finish; the result-sink
    /// receiver is handed back so the test can read what the room
    /// reported at teardown.
    async fn shutdown(mut self) -> mpsc::Receiver<MatchResult> {
        let _ = self.control.send(RoomControl::Shutdown).await;
        self.tick();
        self.tick();
        self.handle.await.unwrap();
        self.results
    }
}

fn action(conn: gsb_core::ConnectionId, op: u16) -> Action {
    // Test identity policy: the conn id doubles as the player id.
    Action {
        conn,
        player: gsb_core::PlayerId(conn.0),
        op,
        payload: bytes::Bytes::new(),
    }
}

/// Under a sustained overload the room's pull budget cannot serve every
/// connection in one tick; with a persistent flooder at the FRONT of the
/// join-order roster, every connection is still reached within N ticks,
/// where N is the roster length (the rotation guarantee). The old fixed
/// HashMap-order scan cannot offer this bound: whichever prefix the hash
/// order picks consumes both budget slots every tick and the tail is
/// never reached.
#[tokio::test]
async fn sustained_overload_reaches_every_connection_within_n_ticks() {
    const N_CONNS: u64 = 6;
    // Per-tick capacity = TWO connections' worth (room budget 2, per-conn
    // budget 1): six connections with pending input can NEVER fit in one
    // tick, which is what makes the scan order decide who is reached.
    let cfg = RoomConfig {
        id: RoomId(1),
        tick_hz: 30.0,
        max_actions_per_conn_per_tick: 1,
        max_pending_actions: 2,
        metrics_cadence_hz: 30.0,
        ..Default::default()
    };
    let mut h = Harness::new(cfg).await;

    // The flooder joins FIRST: front of the join-order roster — the worst
    // starting position for everyone behind it.
    h.join(gsb_core::ConnectionId(1)).await;
    for c in 2..=N_CONNS {
        h.join(gsb_core::ConnectionId(c)).await;
    }

    // The overload: the flooder holds far more than any single tick can
    // pull, so its channel never runs dry during the run; each victim
    // holds enough that an action is always waiting when rotation reaches
    // it. All queues are filled only after every join has settled, and
    // the sample backlog is drained first, so the next step is the first
    // one that sees the filled channels.
    for _ in 0..64 {
        h.actions
            .get(&gsb_core::ConnectionId(1))
            .unwrap()
            .send(action(gsb_core::ConnectionId(1), 0x30))
            .await
            .expect("flood channel alive");
    }
    for c in 2..=N_CONNS {
        for _ in 0..3 {
            h.actions
                .get(&gsb_core::ConnectionId(c))
                .unwrap()
                .send(action(gsb_core::ConnectionId(c), 0x40))
                .await
                .expect("victim channel alive");
        }
    }
    h.sync_steps().await;
    let base_tick = h.next_tick; // last join-phase step; the run starts at +1

    // Exactly N paced steps: each tick is awaited to completion (a fresh
    // sample), so the rotation guarantee's bound is measured exactly.
    for _ in 0..N_CONNS {
        h.tick();
        let _barrier = h.step_done().await;
    }

    // Structural no-drop preserved under overload: work was DEFERRED, not
    // dropped — read from the freshest sample (the drain reaches past any
    // backlog to the last completed step).
    let mut dropped_actions = 0u64;
    while let Ok(ev) = h.metrics_rx.try_recv() {
        if let MetricsEvent::Room(s) = ev {
            dropped_actions = dropped_actions.max(s.dropped_actions);
        }
    }
    assert_eq!(
        dropped_actions, 0,
        "the room never drops an action, not under overload"
    );

    // The ledger rides out through the match-result seam at shutdown.
    let mut results = h.shutdown().await;
    let result = tokio::time::timeout(Duration::from_secs(2), results.recv())
        .await
        .expect("timed out waiting for the match result")
        .expect("result sink closed without the room reporting");
    assert_eq!(result.room, RoomId(1));

    let p = result.payload;
    let n = u32::from_le_bytes(p[..4].try_into().unwrap()) as usize;
    let mut first_seen: HashMap<u64, u64> = HashMap::with_capacity(n);
    for i in 0..n {
        let off = 4 + i * 16;
        let conn = u64::from_le_bytes(p[off..off + 8].try_into().unwrap());
        let tick = u64::from_le_bytes(p[off + 8..off + 16].try_into().unwrap());
        first_seen.insert(conn, tick);
    }

    // THE PROPERTY: every connection — including the five behind the
    // persistent flooder — was reached within N steps after the fill
    // (tick indices in the ledger are global; the bound is relative to
    // `base_tick`).
    for c in 1..=N_CONNS {
        let t = first_seen
            .get(&c)
            .unwrap_or_else(|| panic!("connection {c} was NEVER reached"));
        let reached_within = t - base_tick;
        assert!(
            reached_within <= N_CONNS,
            "connection {c} first reached {} steps after the fill (> {N_CONNS})",
            reached_within
        );
    }

    // The overload was real (a vacuous pass would serve everyone on the
    // first tick): each tick ingests at most 2 actions, so six FIRST
    // ingests require at least three distinct tick indices. No action is
    // ingested before the fill either (the queues were empty during the
    // join ticks), so this holds deterministically.
    let distinct_first_ticks: BTreeSet<u64> = first_seen.values().copied().collect();
    assert!(
        distinct_first_ticks.len() >= 3,
        "the pull budget did not bind — the scenario was not an overload: \
         {first_seen:?}"
    );
}
