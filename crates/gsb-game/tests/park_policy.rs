//! The demo disconnect policy end-to-end through the REAL room actor
//! (`docs/RECONNECT.md` §15 Tur B: MOBA-style park + §9 bot stub):
//!
//! 1. a held disconnect keeps the hero in the world (visible in another
//!    member's snapshots) and keeps its room-cap slot;
//! 2. an expired grace hands the hero to the demo stub bot — which must
//!    exercise the REAL movement path (the entity KEEPS MOVING on
//!    synthesized input, not just standing);
//! 3. a resume reclaims the entity onto the fresh session: same wire id,
//!    bot silenced, human input drives the hero again;
//! 4. `grace = 0` restores the pre-reconnect despawn semantics exactly;
//! 5. the sharded topology carries the park record inside the migrating
//!    state (§14.2) and its bot feeds through the same shared ingest.
//!
//! Harness: the manual-ticker idiom of `gsb-core/tests/reconnect.rs` —
//! the metrics channel carries one sample per step, so a recv is the
//! step-completed barrier. Unlike those core-level tests, the world here
//! is a real bevy World driven by [`gsb_game::room::DemoRoom`] (the
//! actual game), so these are game-band locks: the policy answers, the
//! ledger behavior, and the bot's synthesized input riding the ordinary
//! ingest path.
//!
//! Client-view semantics: snapshots are full and self-contained and the
//! room ships NOTHING while nothing changed, so each test keeps ONE
//! persistent view per observer and applies every newly received
//! snapshot over it (silence = view unchanged — exactly what a real
//! client sees).

use std::collections::HashMap;
use std::time::{Duration, Instant};

use gsb_core::channel::{channel, FrameBatch, Mailbox};
use gsb_core::id::{ConnectionId, EntityId, RoomId};
use gsb_core::metrics::{MetricsEvent, RoomSample};
use gsb_core::room::{Action, Detach, ExpireTo, GameLogic, RoomActor, RoomConfig, RoomControl};
use gsb_core::ticker::TickInfo;
use gsb_game::game::{MoveTo, WorldSnapshot};
use gsb_game::op;
use prost::Message;
use tokio::sync::{mpsc, oneshot};

/// Manual-ticker room harness over the real demo room.
struct H {
    tick_tx: tokio::sync::broadcast::Sender<TickInfo>,
    control: Mailbox<RoomControl>,
    handle: tokio::task::JoinHandle<()>,
    metrics_rx: mpsc::Receiver<MetricsEvent>,
    t0: Instant,
    next_tick: u64,
}

impl H {
    fn new(grace: Duration) -> Self {
        let (tick_tx, tick_rx) = tokio::sync::broadcast::channel(256);
        let (control, control_rx) = channel(128);
        let (metrics_tx, metrics_rx) = mpsc::channel(1024);
        let config = RoomConfig {
            id: RoomId(1),
            tick_hz: 60.0,
            keepalive_hz: 0.0, // strict silence when nothing changed
            // One sample per step: the step barrier below.
            metrics_cadence_hz: 60.0,
            ..Default::default()
        };
        let logic = gsb_game::room::DemoRoom::new().with_disconnect_grace(grace);
        let actor = RoomActor::new(
            config,
            bevy_ecs::world::World::new(),
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

    /// Feed one tick and wait for its sample (the step barrier).
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

    /// The freshest cumulative counters (one extra barriered step).
    async fn latest_sample(&mut self) -> RoomSample {
        self.step().await;
        while self.metrics_rx.try_recv().is_ok() {}
        self.tick();
        match tokio::time::timeout(Duration::from_secs(5), self.metrics_rx.recv())
            .await
            .expect("sample timed out")
            .expect("metrics closed")
        {
            MetricsEvent::Room(s) => s,
            other => panic!("unexpected metrics event {other:?}"),
        }
    }

    async fn join(
        &mut self,
        conn: ConnectionId,
    ) -> (EntityId, Mailbox<Action>, mpsc::Receiver<FrameBatch>) {
        let (out_tx, out_rx) = mpsc::channel::<FrameBatch>(256);
        let (reply_tx, reply_rx) =
            oneshot::channel::<Result<(EntityId, Mailbox<Action>), gsb_core::error::CoreError>>();
        self.control
            .send(RoomControl::Join {
                conn,
                out: out_tx,
                reply: reply_tx,
            })
            .await
            .expect("control alive");
        self.step().await;
        let (entity, actions) = reply_rx.await.expect("reply dropped").expect("join accepted");
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
    ) -> Result<(EntityId, Mailbox<Action>), gsb_core::error::CoreError> {
        let (out_tx, _out_rx) = mpsc::channel::<FrameBatch>(256);
        let (reply_tx, reply_rx) =
            oneshot::channel::<Result<(EntityId, Mailbox<Action>), gsb_core::error::CoreError>>();
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

    async fn shutdown(mut self) {
        let _ = self.control.send(RoomControl::Shutdown).await;
        self.tick();
        tokio::time::timeout(Duration::from_secs(5), self.handle)
            .await
            .expect("room did not shut down")
            .expect("room panicked");
    }
}

/// Apply every NEW world snapshot sitting in `out_rx` to `view` (a full
/// snapshot REPLACES the whole view — the client model). Returns true
/// when at least one fresh snapshot was applied.
async fn apply_snapshots(
    out_rx: &mut mpsc::Receiver<FrameBatch>,
    view: &mut HashMap<u64, (i32, i32)>,
) -> bool {
    let mut fresh = false;
    while let Ok(batch) = out_rx.try_recv() {
        for frame in batch {
            if frame.op == op::WORLD_SNAPSHOT
                && let Ok(snap) = WorldSnapshot::decode(&frame.payload[..])
            {
                *view = snap.entities.iter().map(|e| (e.entity, (e.x, e.y))).collect();
                fresh = true;
            }
        }
    }
    fresh
}

// =====================================================================
// 1 — park: entity + slot survive the transport death (§3/§4)
// =====================================================================

#[tokio::test]
async fn held_disconnect_keeps_hero_visible_and_slot_held() {
    // A grace far beyond the test's wall clock: pure hold, no expiry.
    let mut h = H::new(Duration::from_secs(3600));

    let (hero, _actions, mut hero_out) = h.join(ConnectionId(1)).await;
    let (_obs, _obs_actions, mut obs_rx) = h.join(ConnectionId(2)).await;
    h.steps(2).await;

    let mut view = HashMap::new();
    assert!(apply_snapshots(&mut obs_rx, &mut view).await, "join emits");
    assert!(
        view.contains_key(&hero),
        "both members visible before the drop"
    );
    // Drain the hero's own channel: everything it will EVER receive must
    // be in there NOW — after the detach nothing more may arrive.
    let mut hero_view = HashMap::new();
    apply_snapshots(&mut hero_out, &mut hero_view).await;

    // Transport death WITHOUT a leave → the demo policy parks.
    h.detach(ConnectionId(1), hero, "ana").await;
    h.steps(4).await;

    // The parked hero is STILL in the world view (either freshly shipped
    // or last-known-by-silence — both are the client truth; what must
    // never happen is a snapshot REMOVING it).
    apply_snapshots(&mut obs_rx, &mut view).await;
    assert!(
        view.contains_key(&hero),
        "parked hero vanished from the world: {view:?}"
    );
    // …and its own dead half stayed silent after the detach.
    assert!(
        !apply_snapshots(&mut hero_out, &mut hero_view).await,
        "a parked row ships nothing to its dead half"
    );

    // Gauges: the held slot is still counted, the park is visible.
    let s = h.latest_sample().await;
    assert_eq!(s.members, 2, "parked player holds both slots counted");
    assert_eq!(s.detached, 1, "one instant park");

    h.shutdown().await;
}

// =====================================================================
// 2 — §12.9 ai_handover_bot_ingests_while_detached: the bot plays the
//     hero through the REAL input→movement→snapshot path
// =====================================================================

#[tokio::test]
async fn grace_expiry_hands_over_to_a_wandering_bot() {
    let mut h = H::new(Duration::from_millis(120));

    let (hero, _actions, _hero_out) = h.join(ConnectionId(1)).await;
    let (_obs, _obs_actions, mut obs_rx) = h.join(ConnectionId(2)).await;
    h.steps(2).await;
    let mut view = HashMap::new();
    apply_snapshots(&mut obs_rx, &mut view).await;

    h.detach(ConnectionId(1), hero, "ana").await;
    // Outlive the grace, then let the sweep fire (wall-clock deadline).
    tokio::time::sleep(Duration::from_millis(260)).await;
    h.steps(2).await;

    let s = h.latest_sample().await;
    assert_eq!(s.detach_expired_ai, 1, "expiry landed in the AI bucket");
    assert_eq!(s.detach_expired_despawn, 0);
    assert_eq!(s.members, 2, "the bot holds the hero's slot");

    // The bot feeds: over the following seconds the parked hero MOVES —
    // synthesized wander targets ride ingest → the movement system → the
    // broadcast pass (the full real path; no parallel teleport route).
    let mut positions: Vec<(i32, i32)> = Vec::new();
    for _ in 0..180 {
        h.step().await;
        if apply_snapshots(&mut obs_rx, &mut view).await
            && let Some(pos) = view.get(&hero)
        {
            positions.push(*pos);
        }
    }
    assert!(
        positions.len() >= 2,
        "observer stopped receiving the parked hero"
    );
    assert!(
        positions.windows(2).any(|w| w[0] != w[1]),
        "bot-fed hero never moved: {positions:?}"
    );

    // Expiry fires exactly once (deadline cleared, marker latched).
    h.steps(2).await;
    let s2 = h.latest_sample().await;
    assert_eq!(s2.detach_expired_ai, 1);

    h.shutdown().await;
}

// =====================================================================
// 3 — resume reclaims the entity FROM THE BOT: same wire id, human
//     input works again
// =====================================================================

#[tokio::test]
async fn resume_reclaims_the_parked_hero_and_human_input_works() {
    let mut h = H::new(Duration::from_millis(80)); // short: expiry WILL fire

    let (hero, _old_actions, _hero_out) = h.join(ConnectionId(1)).await;
    let (_obs, _obs_actions, mut obs_rx) = h.join(ConnectionId(2)).await;
    h.steps(2).await;
    let mut view = HashMap::new();
    apply_snapshots(&mut obs_rx, &mut view).await;

    h.detach(ConnectionId(1), hero, "ana").await;
    // Let the grace expire FIRST: the handover happens before any human
    // returns — the hard version of reclaim (resume FROM the bot).
    tokio::time::sleep(Duration::from_millis(220)).await;
    h.steps(2).await;
    let s0 = h.latest_sample().await;
    assert_eq!(s0.detach_expired_ai, 1, "bot owns the hero right now");

    // The human returns: implicit resume (§14.3) — SAME wire id comes
    // back, the core clears the bot marker, the ledger entry goes.
    let (entity2, new_actions) = h
        .resume(ConnectionId(9), 7, "ana")
        .await
        .expect("resume accepted");
    assert_eq!(entity2, hero, "wire id unchanged across bot handover");

    // Human input flows again over the NEW session: drive the hero FAR
    // toward one corner.
    new_actions
        .send(Action {
            conn: ConnectionId(9),
            op: op::MOVE_TO,
            payload: MoveTo {
                x: -40,
                y: -40,
                seq: 1,
            }
            .encode_to_vec()
            .into(),
        })
        .await
        .unwrap();

    // Convergence toward the HUMAN target is the double proof: inputs are
    // processed again AND the bot stopped fighting for the entity (an
    // active wanderer would keep deflecting the hero away from the
    // corner). 900 ticks @60 Hz ≈ 15 s of sim time ≈ 150 units of travel
    // at the default speed — enough from any spawn on the 100×100 map.
    let mut best_dist = f32::MAX;
    for _ in 0..900 {
        h.step().await;
        apply_snapshots(&mut obs_rx, &mut view).await;
        if let Some(&(x, y)) = view.get(&hero) {
            let (dx, dy) = (x + 40, y + 40);
            best_dist = best_dist.min(((dx * dx + dy * dy) as f32).sqrt());
        }
    }
    assert!(
        best_dist < 6.0,
        "resumed hero did not converge to the human target (best {best_dist})"
    );

    let s = h.latest_sample().await;
    assert_eq!(s.resumes, 1, "exactly one accepted resume");
    assert_eq!(s.detached, 0, "the park was consumed");
    assert_eq!(s.members, 2, "no slot churn: reclaim, not leave+join");

    h.shutdown().await;
}

// =====================================================================
// 4 — grace = 0: the byte-for-byte old semantics (disconnect = despawn)
// =====================================================================

#[tokio::test]
async fn zero_grace_despawns_immediately_like_before() {
    let mut h = H::new(Duration::ZERO);

    let (hero, _actions, _out) = h.join(ConnectionId(1)).await;
    let (_obs, _obs_actions, mut obs_rx) = h.join(ConnectionId(2)).await;
    h.steps(1).await;
    let mut view = HashMap::new();
    apply_snapshots(&mut obs_rx, &mut view).await;
    assert!(view.contains_key(&hero));

    h.detach(ConnectionId(1), hero, "ana").await;
    h.steps(2).await;
    apply_snapshots(&mut obs_rx, &mut view).await;
    assert!(
        !view.contains_key(&hero),
        "disabled parking must despawn: {view:?}"
    );
    let s = h.latest_sample().await;
    assert_eq!(s.detached, 0, "nothing was ever parked");

    // And the identity is NOT parked either: a later resume falls back to
    // a transparent fresh join (a NEW wire id), per §5.
    let (fresh, _a2) = h
        .resume(ConnectionId(9), 1, "ana")
        .await
        .expect("fallback join ok");
    assert_ne!(fresh, hero, "must be a fresh entity, not a resurrection");

    h.shutdown().await;
}

// =====================================================================
// 5 — the sharded topology (§14.2): the park record travels INSIDE the
//     migrating state, and the shard-side bot feeds via shared ingest
// =====================================================================

mod sharded_park {
    use super::*;
    use bevy_ecs::prelude::Entity;
    use gsb_core::shard::ShardLogic;
    use gsb_game::components::Position;
    use gsb_game::sharded::ShardedRoom;

    #[test]
    fn park_record_travels_inside_the_migration_state() {
        let mut w0 = bevy_ecs::prelude::World::new();
        let mut w1 = bevy_ecs::prelude::World::new();
        let grace = Duration::from_secs(3600);
        let mut s0 = ShardedRoom::new(0, 4, 50.0).with_disconnect_grace(grace);
        let mut s1 = ShardedRoom::new(1, 4, 50.0).with_disconnect_grace(grace);

        // Join on shard 0, then the transport dies: shard 0 parks.
        let wire = s0.on_join(&mut w0, ConnectionId(1));
        assert!(matches!(
            s0.on_disconnect(&mut w0, ConnectionId(1), "ana"),
            Detach::Hold {
                to: ExpireTo::AiHandover,
                ..
            }
        ));
        assert!(matches!(
            s0.resume_lookup(&w0, "ana"),
            gsb_core::room::ResumeFound::Held(_)
        ));

        // The player's entity migrates WHILE detached (its pending move
        // target kept it walking): force it just across the seam into
        // shard 1's region (grid 2×2 ⇒ shard 1 = x∈[0,50], y∈[-50,0]).
        let mut q = w0.query::<(Entity, &gsb_game::components::WireId)>();
        let (e, _wid) = q.iter(&w0).next().expect("hero entity");
        drop(q);
        w0.entity_mut(e).insert(Position { x: 1.0, y: -10.0 });

        let migrations = s0.collect_migrations(&mut w0, 1);
        assert_eq!(migrations.len(), 1, "crossing reported once");
        let m = &migrations[0];
        assert_eq!(m.wire, wire);
        let Some(park) = &m.state.park else {
            panic!("park record must travel with the migrating state (§14.2)");
        };
        assert_eq!(park.identity, "ana");
        assert_eq!(park.conn, ConnectionId(1));
        assert!(!park.bot, "not yet handed to the bot");

        // The receiving shard installs the record: ITS ledger answers the
        // resume now; the sender's must forget the player entirely.
        s1.on_migrate_in(&mut w1, m.wire, m.state.clone(), m.conn);
        assert!(matches!(
            s1.resume_lookup(&w1, "ana"),
            gsb_core::room::ResumeFound::Held(_)
        ));
        s0.on_migrate_out(&mut w0, wire);
        assert!(
            matches!(
                s0.resume_lookup(&w0, "ana"),
                gsb_core::room::ResumeFound::Never
            ),
            "no stranded park record on the sending shard"
        );

        // Reclaim lands on the receiving shard: same wire id, ledger
        // consumed (these hooks are exactly what the core swap calls).
        s1.on_resume(&mut w1, "ana", ConnectionId(1), ConnectionId(9), wire);
        assert!(matches!(
            s1.resume_lookup(&w1, "ana"),
            gsb_core::room::ResumeFound::Never
        ));
    }

    /// The AI-handover expiry latches the shard-side bot marker, and the
    /// synthesized wander rides the ORDINARY ingest: `ingest` consumes
    /// the action list by design, so the observable is the REAL
    /// `MoveTarget` component write it leaves behind — the same effect a
    /// wire frame produces.
    #[test]
    fn shard_bot_feeds_after_expiry_through_ingest() {
        let mut w = bevy_ecs::prelude::World::new();
        let mut s =
            ShardedRoom::new(0, 1, 50.0).with_disconnect_grace(Duration::from_millis(1));

        let _wire = s.on_join(&mut w, ConnectionId(1));
        assert!(matches!(
            s.on_disconnect(&mut w, ConnectionId(1), "ana"),
            Detach::Hold { .. }
        ));
        s.on_detach_expired(&mut w, ConnectionId(1), ExpireTo::AiHandover);
        assert!(matches!(
            s.resume_lookup(&w, "ana"),
            gsb_core::room::ResumeFound::Held(_)
        ));

        fn targets(w: &mut bevy_ecs::prelude::World) -> Vec<gsb_game::components::MoveTarget> {
            let mut q = w.query::<&gsb_game::components::MoveTarget>();
            q.iter(w).copied().collect()
        }
        assert!(targets(&mut w).is_empty(), "nothing drives the hero yet");

        let ctx = |tick: u64| gsb_core::room::TickCtx {
            room: RoomId(1),
            tick,
            dt: Duration::from_secs_f64(1.0 / 30.0),
        };

        // Off-cadence tick: the gate synthesizes nothing.
        let mut acts: Vec<Action> = Vec::new();
        s.ingest(&mut w, &ctx(29), &mut acts);
        assert!(targets(&mut w).is_empty(), "off-cadence tick must not drive");

        // ON-cadence tick: one synthesized frame → one REAL MoveTarget.
        let mut acts: Vec<Action> = Vec::new();
        s.ingest(&mut w, &ctx(30), &mut acts);
        let ts = targets(&mut w);
        assert_eq!(
            ts.len(),
            1,
            "the synthesized frame drove the REAL ingest path"
        );
        assert!(
            ts[0].x.abs() <= 50.0 && ts[0].y.abs() <= 50.0,
            "the wander target stays on the map: {:?}",
            ts[0]
        );
    }
}
