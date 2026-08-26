//! Unit tests for [`super`] (moved out of the module file so the
//! implementation reads on its own; still a child module, so
//! `use super::*` reaches the parent's private items exactly as
//! before).

use super::*;
use crate::channel::channel;
use gsb_protocol::FrameBody;
use std::time::Duration;

/// Faz 2 resume lock: a resume moves ONLY the binding row. Every
/// other table this actor owns is keyed by the stable [`PlayerId`]
/// and must come through UNTOUCHED (`conns`, `roster`, `roster_pos`,
/// the rotation cursor) — the §14.1 "rename N tables" pass no longer
/// exists, and this test pins its absence structurally (crate-visible
/// field checks, like the roster ones). A one-entry park ledger
/// drives the Held path.
///
/// History note: the pre-Faz-2 form of this test was
/// `rebind_rekeys_every_conn_keyed_table` and asserted the OPPOSITE —
/// that `conns`/`roster`/`roster_pos` were renamed old→new. The
/// assertion changed because the design changed (TRAIT-ARCHITECTURE
/// §5): with player-keyed tables there is nothing to rename, and the
/// invariant worth locking is "resume touches exactly the binding".
struct RebindLogic {
    held: std::collections::HashMap<String, PlayerId>,
    ents: std::collections::HashMap<PlayerId, EntityId>,
    next: u64,
}

// Faz 1 trait split: the shared contract lives on `GameLogic`; this
// logic uses no room-exclusive hook, so its `RoomLogic` impl is empty
// (both exclusive methods have defaults).
impl GameLogic<()> for RebindLogic {
    type GroupKey = ();
    type Strip = ();
    fn snapshot_op(&self) -> u16 {
        0x7400
    }
    fn private_op(&self) -> u16 {
        0x7401
    }
    fn group_of(&self, _w: &(), _p: PlayerId) -> Self::GroupKey {}
    fn snapshot(
        &mut self,
        _w: &mut (),
        _c: &TickCtx,
        _g: &(),
        _borrowed: &[crate::shard::BorderRecord<()>],
        _o: &mut bytes::BytesMut,
    ) -> bool {
        false
    }
    fn on_join(&mut self, _w: &mut (), _conn: ConnectionId) -> Admission {
        self.next += 1;
        let player = PlayerId(self.next);
        self.ents.insert(player, self.next);
        Admission {
            player,
            entity: self.next,
        }
    }
    fn on_leave(&mut self, _w: &mut (), player: PlayerId) {
        self.ents.remove(&player);
    }
    fn ingest(&mut self, _w: &mut (), _c: &TickCtx, a: &mut Vec<Action>) {
        a.clear();
    }
    fn update(&mut self, _w: &mut (), _c: &TickCtx) {}
    fn on_disconnect(
        &mut self,
        _w: &mut (),
        player: PlayerId,
        identity: &str,
    ) -> Detach {
        if self.ents.contains_key(&player) {
            self.held.insert(identity.to_string(), player);
        }
        Detach::Hold {
            grace: None,
            to: ExpireTo::Despawn,
        }
    }
    fn resume_lookup(&self, _w: &(), identity: &str) -> ResumeFound {
        match self.held.get(identity) {
            Some(p) => ResumeFound::Held(*p),
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
        // The player-keyed tables keep their keys across the resume:
        // only the ledger entry is consumed.
        self.held.remove(identity);
    }
}

impl RoomLogic<()> for RebindLogic {}

#[test]
fn resume_rekeys_only_the_binding() {
    let cfg = RoomConfig {
        id: RoomId(31),
        ..Default::default()
    };
    let (_tick_tx, tick_rx) = broadcast::channel(4);
    let (_control, control_rx) = channel(16);
    let mut actor = RoomActor::new(
        cfg,
        (),
        Box::new(RebindLogic {
            held: std::collections::HashMap::new(),
            ents: std::collections::HashMap::new(),
            next: 0,
        }),
        tick_rx,
        control_rx,
        1,
        null_metrics_tx(),
        None,
    );
    // Three members join; remember each session's binding row.
    let mut ents = std::collections::HashMap::new();
    let mut pid_of = std::collections::HashMap::new();
    for c in 1..=3u64 {
        let (out_tx, _o) = mpsc::channel::<FrameBatch>(2);
        let (rtx, mut rrx) = oneshot::channel();
        actor.handle_control(RoomControl::Join {
            conn: ConnectionId(c),
            out: out_tx,
            reply: rtx,
        });
        let (e, _a) = rrx.try_recv().ok().unwrap().unwrap();
        ents.insert(ConnectionId(c), e);
        pid_of.insert(ConnectionId(c), actor.binding[&ConnectionId(c)]);
    }
    // Snapshot the resume-insensitive surface BEFORE the park.
    let roster_before = actor.roster.clone();
    let pos_before = actor.roster_pos.clone();
    let cursor_before = actor.read_cursor;
    // c2's transport dies; policy holds.
    actor.handle_control(RoomControl::Detach {
        conn: ConnectionId(2),
        entity: ents[&ConnectionId(2)],
        identity: "ana".into(),
    });
    assert!(actor.conns[&pid_of[&ConnectionId(2)]].detached, "parked");
    // The resume binds a fresh socket (c9) onto the parked row.
    let (out_tx, _o) = mpsc::channel::<FrameBatch>(2);
    let (rtx, mut rrx) = oneshot::channel();
    actor.handle_control(RoomControl::Resume {
        conn: ConnectionId(9),
        epoch: 1,
        identity: "ana".into(),
        out: out_tx,
        reply: rtx,
    });
    let reply = rrx.try_recv().expect("reply sent synchronously");
    let (entity, _actions) = reply.expect("resume accepted");
    assert_eq!(entity, ents[&ConnectionId(2)], "the SAME wire id comes back");

    let pid = pid_of[&ConnectionId(2)];
    // The binding moved — and it is the ONLY table that did.
    assert!(!actor.binding.contains_key(&ConnectionId(2)), "old row gone");
    assert_eq!(actor.binding[&ConnectionId(9)], pid, "new row, SAME player");
    assert_eq!(actor.conns[&pid].conn, ConnectionId(9), "row re-pointed");
    assert!(!actor.conns[&pid].detached, "rebound row live");
    assert_eq!(
        actor.conns[&pid].entity,
        ents[&ConnectionId(2)],
        "entity/wire id intact"
    );
    // conns kept its key...
    assert!(actor.conns.contains_key(&pid));
    // ...roster untouched (same ids, same order, same positions)...
    assert_eq!(actor.roster, roster_before, "roster not touched by resume");
    assert_eq!(actor.roster_pos, pos_before, "position map ditto");
    assert_eq!(actor.read_cursor, cursor_before, "rotation cursor ditto");
    assert_eq!(actor.roster.len(), 3, "no membership change happened");
    // The rebound tables still drain cleanly (mass-leave invariant):
    // leaves arrive under BOTH session ids over the lifetime — each
    // resolves through the CURRENT binding (c2's leaf is stale and
    // must be a no-op now that c9 owns the park).
    for c in [1u64, 3] {
        actor.handle_control(RoomControl::Leave {
            conn: ConnectionId(c),
            entity: ents[&ConnectionId(c)],
        });
    }
    actor.handle_control(RoomControl::Leave {
        conn: ConnectionId(9),
        entity: ents[&ConnectionId(2)],
    });
    assert!(
        actor.roster.is_empty() && actor.roster_pos.is_empty() && actor.conns.is_empty(),
        "drained"
    );
    assert!(actor.binding.is_empty(), "bindings torn down with the rows");
}

/// Regression lock for the roster-drift panic: `swap_remove` returns
/// the REMOVED element, and the fix must retarget the RELOCATED one.
/// The old code fixed the removed element's (just-deleted) entry, so
/// the first non-tail leave left the relocated connection's position
/// stale and a mass-leave run panicked inside `roster_remove` —
/// observed on every loadgen end-of-run (surfaced by the supervision
/// round's death-reaping, which turned the silent task death into a
/// visible warn + reaped room).
#[test]
fn roster_stays_synchronized_through_mass_leaves() {
    let cfg = RoomConfig {
        id: RoomId(1),
        tick_hz: 30.0,
        metrics_cadence_hz: 30.0,
        ..Default::default()
    };
    let (tick_tx, _first) = broadcast::channel(64);
    let (_control, control_rx) = channel(1024);
    let (dts_tx, _d) = mpsc::channel(1);
    let (ops_tx, _o) = mpsc::channel(1);
    let mut actor = RoomActor::new(
        cfg,
        (),
        Box::new(RecLogic {
            dts: dts_tx,
            ops: ops_tx,
        }),
        tick_tx.subscribe(),
        control_rx,
        1,
        null_metrics_tx(),
        None,
    );
    const N: u64 = 50;
    for c in 1..=N {
        let (out_tx, _o) = mpsc::channel(8);
        let (rtx, _rrx) = oneshot::channel();
        actor.handle_control(RoomControl::Join {
            conn: ConnectionId(c),
            out: out_tx,
            reply: rtx,
        });
    }
    assert_eq!(actor.roster.len(), N as usize);
    // Every connection leaves, in join order — the worst pattern for
    // the old code (every removal relocates someone whose position
    // entry then had to be fixed).
    for c in 1..=N {
        actor.handle_control(RoomControl::Leave {
            conn: ConnectionId(c),
            entity: 1,
        });
    }
    assert!(actor.roster.is_empty(), "roster drained");
    assert!(actor.roster_pos.is_empty(), "positions drained");
    assert!(actor.conns.is_empty(), "table drained");
    // And the room still accepts joins afterwards (the structures
    // are consistent, not merely empty).
    let (out_tx, _o) = mpsc::channel(8);
    let (rtx, rrx) = oneshot::channel();
    actor.handle_control(RoomControl::Join {
        conn: ConnectionId(N + 1),
        out: out_tx,
        reply: rtx,
    });
    assert!(
        tokio_sync_oneshot_peek(rrx).is_some(),
        "post-mass-leave join accepted"
    );
}

/// Synchronous peek helper for the test above (the reply was already
/// sent by `handle_control`; a blocking recv would need a runtime).
fn tokio_sync_oneshot_peek(
    mut rx: oneshot::Receiver<Result<(EntityId, Mailbox<Action>), CoreError>>,
) -> Option<Result<(EntityId, Mailbox<Action>), CoreError>> {
    rx.try_recv().ok()
}

/// A metrics sender whose receiver is dropped immediately: the room's
/// per-step send fails and is ignored (the metric path is covered by
/// the dedicated metrics-flow test and by gsb-server's tests).
fn null_metrics_tx() -> mpsc::Sender<MetricsEvent> {
    let (tx, _rx) = mpsc::channel(1);
    tx
}

/// Test logic recording the dt of every step over a channel (no locks:
/// this crate's lint forbids them even in tests).
struct RecLogic {
    dts: mpsc::Sender<Duration>,
    ops: mpsc::Sender<u16>,
}

impl GameLogic<()> for RecLogic {
    type GroupKey = ();
    type Strip = ();

    fn snapshot_op(&self) -> u16 {
        0x7000
    }
    fn private_op(&self) -> u16 {
        0x7001
    }

    fn group_of(&self, _w: &(), _p: PlayerId) -> Self::GroupKey {
        Default::default()
    }

    fn snapshot(
        &mut self,
        _w: &mut (),
        _c: &TickCtx,
        _g: &Self::GroupKey,
        _borrowed: &[crate::shard::BorderRecord<()>],
        _o: &mut bytes::BytesMut,
    ) -> bool {
        false
    }

    fn on_join(&mut self, _w: &mut (), c: ConnectionId) -> Admission {
        // Test identity policy: the conn id doubles as the player id.
        Admission {
            player: PlayerId(c.0),
            entity: 1,
        }
    }
    fn on_leave(&mut self, _w: &mut (), _p: PlayerId) {}
    fn ingest(&mut self, _w: &mut (), _c: &TickCtx, actions: &mut Vec<Action>) {
        for a in actions.drain(..) {
            let _ = self.ops.try_send(a.op);
        }
    }
    fn update(&mut self, _w: &mut (), ctx: &TickCtx) {
        let _ = self.dts.try_send(ctx.dt);
    }
}

// Faz 1 trait split: shared contract on `GameLogic`; no room-exclusive
// hook used (empty `RoomLogic` impl).
impl RoomLogic<()> for RecLogic {}

struct Harness {
    tick_tx: broadcast::Sender<TickInfo>,
    control: Mailbox<RoomControl>,
    handle: tokio::task::JoinHandle<()>,
    t0: Instant,
    next_tick: u64,
    run_every: u64,
}

impl Harness {
    fn new(run_every: u64, config: RoomConfig, logic: RecLogic) -> Self {
        let (tick_tx, _first) = broadcast::channel(64);
        let tick_rx = tick_tx.subscribe();
        let (control, control_rx) = channel(config.control_capacity);
        let actor = RoomActor::new(
            config,
            (),
            Box::new(logic),
            tick_rx,
            control_rx,
            run_every,
            null_metrics_tx(),
            None,
        );
        Self {
            tick_tx,
            control,
            handle: tokio::spawn(actor.run()),
            t0: Instant::now(),
            next_tick: 0,
            run_every: run_every.max(1),
        }
    }

    /// Send the next global tick with an exact synthetic timestamp:
    /// `at = t0 + n * period`, so dts are deterministic.
    fn tick(&mut self, period: Duration) {
        self.next_tick += 1;
        let at =
            self.t0 + Duration::from_secs_f64(self.next_tick as f64 * period.as_secs_f64());
        self.tick_tx
            .send(TickInfo {
                tick: self.next_tick,
                at,
            })
            .expect("room subscriber alive");
    }

    async fn join(
        &mut self,
        conn: ConnectionId,
        ticks_needed: u64,
    ) -> (EntityId, Mailbox<Action>) {
        let (out_tx, _out_rx) = mpsc::channel::<FrameBatch>(8);
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
        // Control is processed on the room's next *step*; feed enough
        // ticks to guarantee one (run_every + slack).
        for _ in 0..ticks_needed {
            self.tick(Duration::from_secs_f64(1.0 / 30.0));
        }
        let (entity, actions) = tokio::time::timeout(Duration::from_secs(2), reply_rx)
            .await
            .expect("timed out waiting for join reply")
            .expect("join reply dropped")
            .expect("join accepted (room not full)");
        (entity, actions)
    }

    async fn shutdown(mut self) {
        self.control
            .send(RoomControl::Shutdown)
            .await
            .expect("control alive");
        // Control is processed on the room's next step: feed enough
        // ticks to guarantee one.
        for _ in 0..self.run_every {
            self.tick(Duration::from_secs_f64(1.0 / 30.0));
        }
        tokio::time::timeout(Duration::from_secs(2), &mut self.handle)
            .await
            .expect("room did not shut down")
            .expect("room task panicked");
    }
}

#[tokio::test]
async fn room_steps_on_ticks_and_pulls_actions() {
    let (dt_tx, mut dts) = mpsc::channel(16);
    let (op_tx, mut ops) = mpsc::channel(16);
    let mut h = Harness::new(
        1,
        RoomConfig {
            id: RoomId(1),
            ..Default::default()
        },
        RecLogic {
            dts: dt_tx,
            ops: op_tx,
        },
    );
    let period = Duration::from_secs_f64(1.0 / 30.0);

    let (entity, actions) = h.join(ConnectionId(7), 1).await;
    assert_eq!(entity, 1);

    // Actions flow over the per-connection channel and are pulled at
    // the next step.
    actions
        .send(Action {
            conn: ConnectionId(7),
            player: PlayerId(7),
            op: 0x1001,
            payload: bytes::Bytes::new(),
        })
        .await
        .unwrap();
    h.tick(period);
    h.tick(period);

    // 3 steps so far (one from the join helper, two here): dts are the
    // exact nominal period (synthetic timestamps).
    for _ in 0..3 {
        let dt = tokio::time::timeout(Duration::from_secs(2), dts.recv())
            .await
            .expect("timed out")
            .expect("dts closed");
        assert!(
            dt.abs_diff(period) < Duration::from_micros(1),
            "dt {dt:?} != period {period:?}"
        );
    }
    let op = tokio::time::timeout(Duration::from_secs(2), ops.recv())
        .await
        .expect("timed out")
        .expect("ops closed");
    assert_eq!(op, 0x1001);

    h.shutdown().await;
}

#[tokio::test]
async fn catchup_clamps_dt_after_long_gap() {
    let (dt_tx, mut dts) = mpsc::channel(16);
    let (op_tx, _ops) = mpsc::channel(16);
    let mut h = Harness::new(
        1,
        RoomConfig {
            id: RoomId(1),
            ..Default::default()
        }, // max_catchup = 4
        RecLogic {
            dts: dt_tx,
            ops: op_tx,
        },
    );
    let period = Duration::from_secs_f64(1.0 / 30.0);

    // Two normal steps, then a 2 s gap: the step's dt must be clamped
    // to 4 periods (frame-rate independence in steady state; bounded
    // slow-motion across the stall).
    h.tick(period);
    h.tick(period);
    h.next_tick += 1; // consume index 3 as "missed"
    let at = h.t0 + period * 4 + Duration::from_secs(2);
    h.tick_tx
        .send(TickInfo { tick: 4, at })
        .expect("subscriber alive");

    let first = dts.recv().await.expect("dts");
    let second = dts.recv().await.expect("dts");
    let third = dts.recv().await.expect("dts");
    assert!(first.abs_diff(period) < Duration::from_micros(1));
    assert!(second.abs_diff(period) < Duration::from_micros(1));
    assert!(
        third.abs_diff(period * 4) < Duration::from_micros(1),
        "clamped dt {third:?} != 4 * period {period:?}"
    );

    h.shutdown().await;
}

#[tokio::test]
async fn slower_room_steps_on_every_kth_global_tick() {
    let (dt_tx, mut dts) = mpsc::channel(16);
    let (op_tx, _ops) = mpsc::channel(16);
    // Room at 15 Hz under a 60 Hz global ticker: run_every = 4.
    let mut h = Harness::new(
        4,
        RoomConfig {
            id: RoomId(1),
            tick_hz: 15.0,
            ..Default::default()
        },
        RecLogic {
            dts: dt_tx,
            ops: op_tx,
        },
    );
    let global_period = Duration::from_secs_f64(1.0 / 60.0);

    for _ in 0..8 {
        h.tick(global_period);
    }

    // Steps happened on ticks 4 and 8 only: 2 dts of 4 global periods.
    let room_period = Duration::from_secs_f64(1.0 / 15.0);
    for _ in 0..2 {
        let dt = tokio::time::timeout(Duration::from_secs(2), dts.recv())
            .await
            .expect("timed out")
            .expect("dts closed");
        assert!(
            dt.abs_diff(room_period) < Duration::from_micros(1),
            "dt {dt:?} != room period {room_period:?}"
        );
    }

    h.shutdown().await;
}

#[tokio::test]
async fn lagged_receiver_catches_up_and_keeps_stepping() {
    let (dt_tx, mut dts) = mpsc::channel(16);
    let (op_tx, _ops) = mpsc::channel(16);
    // Buffer of 2: flooding it makes the receiver lag deterministically
    // *before* the room starts consuming.
    let (tick_tx, lagged_rx) = broadcast::channel(2);
    let (_control, control_rx) = channel(16);
    let t0 = Instant::now();
    let period = Duration::from_secs_f64(1.0 / 30.0);
    for i in 1..=10u64 {
        tick_tx
            .send(TickInfo {
                tick: i,
                at: t0 + Duration::from_secs_f64(i as f64 * period.as_secs_f64()),
            })
            .expect("channel open");
    }
    let actor = RoomActor::new(
        RoomConfig {
            id: RoomId(1),
            ..Default::default()
        },
        (),
        Box::new(RecLogic {
            dts: dt_tx,
            ops: op_tx,
        }),
        lagged_rx,
        control_rx,
        1,
        null_metrics_tx(),
            None,
    );
    let handle = tokio::spawn(actor.run());

    // The room skips the lagged ticks (Lagged → continue) and steps on
    // the two still-buffered ticks (9 and 10), then the sender is
    // dropped → Closed → clean exit.
    drop(tick_tx);
    tokio::time::timeout(Duration::from_secs(2), handle)
        .await
        .expect("room did not exit on closed ticker")
        .expect("room task panicked");
    let mut count = 0;
    while let Ok(dt) = dts.try_recv() {
        count += 1;
        assert!(dt <= period * 2);
    }
    assert_eq!(count, 2, "expected exactly the two buffered ticks");
}

#[tokio::test]
async fn room_exits_when_ticker_closes() {
    let (dt_tx, _dts) = mpsc::channel(16);
    let (op_tx, _ops) = mpsc::channel(16);
    let (tick_tx, tick_rx) = broadcast::channel(4);
    let (_control, control_rx) = channel(16);
    let actor = RoomActor::new(
        RoomConfig {
            id: RoomId(1),
            ..Default::default()
        },
        (),
        Box::new(RecLogic {
            dts: dt_tx,
            ops: op_tx,
        }),
        tick_rx,
        control_rx,
        1,
        null_metrics_tx(),
            None,
    );
    let handle = tokio::spawn(actor.run());
    drop(tick_tx); // ticker aborted → broadcast closes
    tokio::time::timeout(Duration::from_secs(2), handle)
        .await
        .expect("room did not exit on closed ticker")
        .expect("room task panicked");
}

#[test]
fn config_period() {
    let c = RoomConfig {
        tick_hz: 30.0,
        ..Default::default()
    };
    assert!((c.period().as_secs_f64() - 1.0 / 30.0).abs() < 1e-9);
}

/// `period` is total for hand-built configs: every rate without a
/// usable period yields the typed-in-spirit fallback instead of the
/// `Duration::from_secs_f64` panic (mirrors ticker.rs's
/// `spawn_rejects_rates_without_a_period`; the registry rejects these
/// configs, but direct construction bypasses it — see `period`'s
/// docs). An absurdly HIGH rate truncates its sub-nanosecond period
/// to zero and falls back too (a zero period would divide-by-zero
/// every cadence derivation downstream).
#[test]
fn period_is_total_for_hand_built_configs() {
    for bad in [0.0, -30.0, f64::NAN, f64::INFINITY, f64::NEG_INFINITY, 1e15] {
        let c = RoomConfig {
            tick_hz: bad,
            ..Default::default()
        };
        assert_eq!(
            c.period(),
            FALLBACK_TICK_PERIOD,
            "tick_hz = {bad} must fall back, not panic"
        );
    }
    // A normal rate is untouched by the guard.
    let c = RoomConfig {
        tick_hz: 60.0,
        ..Default::default()
    };
    assert!((c.period().as_secs_f64() - 1.0 / 60.0).abs() < 1e-9);
}

// -----------------------------------------------------------------
// Group machinery: per-connection groups, private frames, silence on
// no-change, keep-alive re-send.
// -----------------------------------------------------------------

/// Test logic that exercises the group machinery end to end:
/// - `GroupKey = PlayerId`: every player is its own group, so a
///   group's snapshot must never reach another player;
/// - emission is gated on a per-group dirty set that membership
///   changes (join/leave) set — per the room contract, a join/leave
///   **is** a change (for its own group); a clean group reports "no
///   change" and nothing is sent (except the room's keep-alive re-send);
/// - `private` emits a frame for exactly one designated player.
struct GroupLogic {
    player_entity: HashMap<PlayerId, u64>,
    next: u64,
    dirty: std::collections::HashSet<PlayerId>,
    step_no: u64,
    steps: mpsc::Sender<u64>,
}

impl GameLogic<()> for GroupLogic {
    type GroupKey = PlayerId;
    type Strip = ();

    fn snapshot_op(&self) -> u16 {
        0x7010
    }
    fn private_op(&self) -> u16 {
        0x7011
    }

    fn group_of(&self, _w: &(), player: PlayerId) -> Self::GroupKey {
        player
    }

    fn snapshot(
        &mut self,
        _w: &mut (),
        _c: &TickCtx,
        group: &Self::GroupKey,
        _borrowed: &[crate::shard::BorderRecord<()>],
        out: &mut bytes::BytesMut,
    ) -> bool {
        if !self.dirty.remove(group) {
            return false; // unchanged since the last emission
        }
        let entity = self.player_entity.get(group).copied().unwrap_or(0);
        out.extend_from_slice(&entity.to_le_bytes());
        true
    }

    fn private(
        &mut self,
        _w: &mut (),
        player: PlayerId,
        _group: &PlayerId,
        _responses: &[crate::rpc::RpcReply],
        out: &mut bytes::BytesMut,
    ) -> bool {
        if player == PlayerId(0x70) {
            out.extend_from_slice(&u32::MAX.to_le_bytes());
            true
        } else {
            false
        }
    }

    fn on_join(&mut self, _w: &mut (), conn: ConnectionId) -> Admission {
        self.next += 1;
        // Test identity policy: the conn id doubles as the player id
        // (and therefore as this logic's per-player group key).
        let player = PlayerId(conn.0);
        self.player_entity.insert(player, self.next);
        self.dirty.insert(player); // membership changed (this group)
        Admission {
            player,
            entity: self.next,
        }
    }
    fn on_leave(&mut self, _w: &mut (), player: PlayerId) {
        self.player_entity.remove(&player);
        // The leaver's group is gone (and clean); the remaining groups
        // are unchanged for a per-player grouping.
        self.dirty.remove(&player);
    }
    fn ingest(&mut self, _w: &mut (), _c: &TickCtx, actions: &mut Vec<Action>) {
        actions.clear();
    }
    fn update(&mut self, _w: &mut (), _c: &TickCtx) {
        self.step_no += 1;
        let _ = self.steps.try_send(self.step_no);
    }
}

// Faz 1 trait split: shared contract on `GameLogic`; no room-exclusive
// hook used (empty `RoomLogic` impl).
impl RoomLogic<()> for GroupLogic {}

/// Manual-ticker harness for `GroupLogic` rooms.
struct GLRoom {
    tick_tx: broadcast::Sender<TickInfo>,
    control: Mailbox<RoomControl>,
    handle: tokio::task::JoinHandle<()>,
    t0: Instant,
    next_tick: u64,
}

impl GLRoom {
    fn new(config: RoomConfig, logic: GroupLogic) -> Self {
        let (tick_tx, tick_rx) = broadcast::channel(64);
        let (control, control_rx) = channel(config.control_capacity);
        let actor = RoomActor::new(
            config,
            (),
            Box::new(logic),
            tick_rx,
            control_rx,
            1,
            null_metrics_tx(),
            None,
        );
        Self {
            tick_tx,
            control,
            handle: tokio::spawn(actor.run()),
            t0: Instant::now(),
            next_tick: 0,
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

    async fn join(
        &mut self,
        conn: ConnectionId,
    ) -> (EntityId, mpsc::Receiver<FrameBatch>) {
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
        self.tick();
        let (entity, _actions) = tokio::time::timeout(Duration::from_secs(2), reply_rx)
            .await
            .expect("timed out waiting for join reply")
            .expect("join reply dropped")
            .expect("join accepted (room not full)");
        (entity, out_rx)
    }

    async fn leave(&mut self, conn: ConnectionId, entity: EntityId) {
        self.control
            .send(RoomControl::Leave { conn, entity })
            .await
            .expect("control alive");
        self.tick();
    }

    /// Feed one tick and wait until the room has stepped it (step
    /// counter from the logic).
    async fn step(&mut self) {
        self.tick();
    }

    async fn shutdown(mut self) {
        self.control
            .send(RoomControl::Shutdown)
            .await
            .expect("control alive");
        self.tick();
        tokio::time::timeout(Duration::from_secs(2), &mut self.handle)
            .await
            .expect("room did not shut down")
            .expect("room task panicked");
    }
}

/// Wait until the logic reports `step_no` steps, then give the room a
/// moment to finish the in-flight step's fan-out (the step counter is
/// emitted in phase 3, fan-out is phase 4; the sleep is generous —
/// fan-out is microsecond-scale).
async fn wait_steps(steps: &mut mpsc::Receiver<u64>, n: u64) {
    while let Some(s) = tokio::time::timeout(Duration::from_secs(2), steps.recv())
        .await
        .expect("steps closed")
    {
        if s == n {
            tokio::time::sleep(Duration::from_millis(50)).await;
            return;
        }
    }
    panic!("steps channel closed before step {n}");
}

fn batch_frames(batch: &[FrameBody]) -> Vec<(u16, Vec<u8>)> {
    batch
        .iter()
        .map(|f| (f.op, f.payload.to_vec()))
        .collect()
}

#[tokio::test]
async fn per_connection_groups_isolate_snapshots_and_private() {
    let (step_tx, mut steps) = mpsc::channel(64);
    let mut room = GLRoom::new(
        RoomConfig {
            id: RoomId(1),
            ..Default::default()
        }, // keep-alive 1 Hz at 30 Hz: never due in this test's window
        GroupLogic {
            player_entity: HashMap::new(),
            next: 0,
            dirty: std::collections::HashSet::new(),
            step_no: 0,
            steps: step_tx,
        },
    );

    let (a_ent, mut a_rx) = room.join(ConnectionId(1)).await;
    let (b_ent, mut b_rx) = room.join(ConnectionId(2)).await;
    let (c_ent, mut c_rx) = room.join(ConnectionId(0x70)).await; // private target

    // Each join dirties exactly its own group (a per-connection
    // grouping: B joining changes nothing for A). So by step 3 each
    // connection's queue holds exactly its own single snapshot — and
    // never another connection's group content.
    wait_steps(&mut steps, 3).await;
    let a_all = drain_all(&mut a_rx).await;
    let b_all = drain_all(&mut b_rx).await;
    let c_all = drain_all(&mut c_rx).await;
    assert_eq!(a_all.len(), 1, "A emitted once (its own join)");
    assert_eq!(b_all.len(), 1, "B emitted once (its own join)");
    assert_eq!(c_all.len(), 1, "C emitted once (its own join)");
    assert_eq!(
        batch_frames(&a_all[0]),
        vec![(0x7010, a_ent.to_le_bytes().to_vec())],
        "A must see exactly its own group's snapshot"
    );
    assert_eq!(
        batch_frames(&b_all[0]),
        vec![(0x7010, b_ent.to_le_bytes().to_vec())],
        "B must see exactly its own group's snapshot"
    );
    assert_eq!(
        batch_frames(&c_all[0]),
        vec![
            (0x7010, c_ent.to_le_bytes().to_vec()),
            (0x7011, u32::MAX.to_le_bytes().to_vec())
        ],
        "the private frame goes only to the designated connection"
    );

    // C leaves: for a per-connection grouping the remaining groups are
    // unchanged, so nothing is re-emitted.
    room.leave(ConnectionId(0x70), c_ent).await;
    wait_steps(&mut steps, 4).await;
    assert!(drain_all(&mut a_rx).await.is_empty(), "A unchanged ⇒ no batch");
    assert!(drain_all(&mut b_rx).await.is_empty(), "B unchanged ⇒ no batch");

    // Nothing changed anymore: no snapshot is emitted at all.
    room.step().await;
    room.step().await;
    wait_steps(&mut steps, 6).await;
    assert!(
        drain_all(&mut a_rx).await.is_empty(),
        "no change (and no keep-alive due) ⇒ no batch"
    );
    assert!(
        drain_all(&mut b_rx).await.is_empty(),
        "no change ⇒ no batch"
    );

    room.shutdown().await;
}

/// Test logic for the "the world changes on every tick" scenario,
/// with contract-conforming bookkeeping: each group remembers the
/// world step *it* last emitted at, keyed by the group (the
/// `GameLogic::snapshot` contract's per-group requirement). A group
/// whose content is the whole world must emit on every tick.
struct FairLogic {
    last_world: u64,
    last_emitted: HashMap<PlayerId, u64>,
    step_no: u64,
    steps: mpsc::Sender<u64>,
}

impl GameLogic<()> for FairLogic {
    type GroupKey = PlayerId;
    type Strip = ();

    fn snapshot_op(&self) -> u16 {
        0x7020
    }
    fn private_op(&self) -> u16 {
        0x7021
    }

    fn group_of(&self, _w: &(), player: PlayerId) -> Self::GroupKey {
        player
    }

    fn snapshot(
        &mut self,
        _w: &mut (),
        _c: &TickCtx,
        group: &Self::GroupKey,
        _borrowed: &[crate::shard::BorderRecord<()>],
        out: &mut bytes::BytesMut,
    ) -> bool {
        // "Unchanged" = the world step this group emitted at is the
        // current one. The map is keyed by `group` — per-group
        // bookkeeping, so one group's emission cannot make another
        // group's answer change in the same tick.
        if self.last_emitted.get(group).copied() == Some(self.last_world) {
            return false;
        }
        self.last_emitted.insert(*group, self.last_world);
        out.extend_from_slice(&self.last_world.to_le_bytes());
        true
    }

    fn on_join(&mut self, _w: &mut (), conn: ConnectionId) -> Admission {
        // Test identity policy: the conn id doubles as the player id.
        Admission {
            player: PlayerId(conn.0),
            entity: 1,
        }
    }
    fn on_leave(&mut self, _w: &mut (), _player: PlayerId) {}
    fn ingest(&mut self, _w: &mut (), _c: &TickCtx, actions: &mut Vec<Action>) {
        actions.clear();
    }
    fn update(&mut self, _w: &mut (), _c: &TickCtx) {
        // The world changes on every tick (e.g. one entity moving).
        self.last_world += 1;
        self.step_no += 1;
        let _ = self.steps.try_send(self.step_no);
    }
}

// Faz 1 trait split: shared contract on `GameLogic`; no room-exclusive
// hook used (empty `RoomLogic` impl).
impl RoomLogic<()> for FairLogic {}

#[tokio::test]
async fn all_dirty_groups_emit_on_the_same_tick() {
    // The external-measurement scenario with contract-conforming
    // (per-group) bookkeeping: two per-connection groups whose
    // content is the whole world, and the world changes on every
    // tick. Every group must emit on every tick — the group visited
    // first by the room must not make the later groups see "no
    // change" (that is exactly what a ledger shared across groups
    // does; the `GameLogic::snapshot` contract forbids it).
    let (step_tx, mut steps) = mpsc::channel(64);
    let (tick_tx, tick_rx) = broadcast::channel(64);
    let (control, control_rx) = channel(128);
    let actor = RoomActor::new(
        RoomConfig {
            id: RoomId(3),
            ..Default::default()
        }, // keep-alive 1 Hz at 30 Hz: never due in this test's window
        (),
        Box::new(FairLogic {
            last_world: 0,
            last_emitted: HashMap::new(),
            step_no: 0,
            steps: step_tx,
        }),
        tick_rx,
        control_rx,
        1,
        null_metrics_tx(),
            None,
    );
    let handle = tokio::spawn(actor.run());
    let t0 = Instant::now();
    let mut next_tick = 0;
    let mut tick = || {
        next_tick += 1;
        let at = t0 + Duration::from_secs_f64(next_tick as f64 / 30.0);
        tick_tx
            .send(TickInfo {
                tick: next_tick,
                at,
            })
            .expect("room subscriber alive");
    };

    // conn 1 joins (tick 1); the control is processed on the room's
    // next step, so the tick goes out before the reply is awaited.
    let (out1_tx, mut a_rx) = mpsc::channel::<FrameBatch>(64);
    let (reply1_tx, reply1_rx) =
        oneshot::channel::<Result<(EntityId, Mailbox<Action>), CoreError>>();
    control
        .send(RoomControl::Join {
            conn: ConnectionId(1),
            out: out1_tx,
            reply: reply1_tx,
        })
        .await
        .expect("control alive");
    tick();
    tokio::time::timeout(Duration::from_secs(2), reply1_rx)
        .await
        .expect("join reply timeout")
        .expect("join reply dropped")
        .expect("join accepted (room not full)");

    // conn 2 joins (tick 2).
    let (out2_tx, mut b_rx) = mpsc::channel::<FrameBatch>(64);
    let (reply2_tx, reply2_rx) =
        oneshot::channel::<Result<(EntityId, Mailbox<Action>), CoreError>>();
    control
        .send(RoomControl::Join {
            conn: ConnectionId(2),
            out: out2_tx,
            reply: reply2_tx,
        })
        .await
        .expect("control alive");
    tick();
    tokio::time::timeout(Duration::from_secs(2), reply2_rx)
        .await
        .expect("join reply timeout")
        .expect("join reply dropped")
        .expect("join accepted (room not full)");

    // 25-tick window: the world changes on every tick, so BOTH
    // groups are dirty on every tick.
    for _ in 0..25 {
        tick();
    }
    wait_steps(&mut steps, 27).await;

    let a_all = drain_all(&mut a_rx).await;
    let b_all = drain_all(&mut b_rx).await;
    // A: its join tick (world 1) + B's join tick (world 2) + all 25
    // window ticks. B: its join tick + all 25 window ticks.
    let seq = |batches: &Vec<Vec<FrameBody>>| {
        batches
            .iter()
            .map(|b| {
                u64::from_le_bytes(
                    b[0]
                        .payload
                        .get(0..8)
                        .expect("8-byte payload")
                        .try_into()
                        .expect("8-byte payload"),
                )
            })
            .collect::<Vec<u64>>()
    };
    let a_seq = seq(&a_all);
    let b_seq = seq(&b_all);
    assert_eq!(
        a_seq,
        (1..=27).collect::<Vec<_>>(),
        "A must emit on every tick the world changed"
    );
    assert_eq!(
        b_seq,
        (2..=27).collect::<Vec<_>>(),
        "B must emit on every tick the world changed (no starvation)"
    );

    control
        .send(RoomControl::Shutdown)
        .await
        .expect("control alive");
    tick();
    tokio::time::timeout(Duration::from_secs(2), handle)
        .await
        .expect("room did not shut down")
        .expect("room task panicked");
}

/// Batch-buffer reuse (the floor turn): the fan-out hands each tick's
/// batch to the outbound channel with `mem::take` and, on a full
/// channel, restores it through `TrySendError::into_inner`. This
/// locks the recovery path: after a stretch in which the outbound
/// channel stayed full (emissions dropped), the channel must hold
/// exactly its capacity of intact batches — and, once space appears,
/// the NEXT emission must arrive (no wedged connection, no lost
/// buffer, nothing past capacity).
#[tokio::test]
async fn full_outbound_channel_drops_then_recovers() {
    // Wider steps pipe than the test's tick count: the room's
    // `try_send` on it is best-effort (a full pipe would silently
    // lose the late step numbers and starve `wait_steps`).
    let (step_tx, mut steps) = mpsc::channel(256);
    let (tick_tx, tick_rx) = broadcast::channel(64);
    let (control, control_rx) = channel(128);
    let actor = RoomActor::new(
        RoomConfig {
            id: RoomId(4),
            ..Default::default()
        },
        (),
        Box::new(FairLogic {
            last_world: 0,
            last_emitted: HashMap::new(),
            step_no: 0,
            steps: step_tx,
        }),
        tick_rx,
        control_rx,
        1,
        null_metrics_tx(),
            None,
    );
    let handle = tokio::spawn(actor.run());
    let t0 = Instant::now();
    let mut next_tick = 0;
    let mut tick = || {
        next_tick += 1;
        let at = t0 + Duration::from_secs_f64(next_tick as f64 / 30.0);
        tick_tx
            .send(TickInfo {
                tick: next_tick,
                at,
            })
            .expect("room subscriber alive");
    };

    // One connection, an outbound channel of capacity 64.
    let (out_tx, mut rx) = mpsc::channel::<FrameBatch>(64);
    let (reply_tx, reply_rx) =
        oneshot::channel::<Result<(EntityId, Mailbox<Action>), CoreError>>();
    control
        .send(RoomControl::Join {
            conn: ConnectionId(1),
            out: out_tx,
            reply: reply_tx,
        })
        .await
        .expect("control alive");
    tick();
    tokio::time::timeout(Duration::from_secs(2), reply_rx)
        .await
        .expect("join reply timeout")
        .expect("join reply dropped")
        .expect("join accepted (room not full)");

    // The world changes on every tick ⇒ one emission per tick:
    // 69 more ticks ⇒ 70 emissions total against 64 slots. The
    // channel keeps the OLDEST 64 (new ones fail `try_send` and are
    // dropped — the documented one-snapshot-of-staleness cost).
    // Yield between sends: on a current-thread runtime the room task
    // cannot consume the broadcast while the test runs, and a full
    // broadcast buffer would overwrite the oldest ticks.
    for _ in 0..69 {
        tick();
        tokio::task::yield_now().await;
    }
    wait_steps(&mut steps, 70).await;

    let all = drain_all(&mut rx).await;
    assert_eq!(all.len(), 64, "the channel holds exactly its capacity");
    let seq: Vec<u64> = all
        .iter()
        .map(|b| {
            assert_eq!(b[0].op, 0x7020, "snapshot opcode");
            u64::from_le_bytes(
                b[0]
                    .payload
                    .get(0..8)
                    .expect("8-byte payload")
                    .try_into()
                    .expect("8-byte payload"),
            )
        })
        .collect();
    assert_eq!(seq, (1..=64).collect::<Vec<_>>(), "intact, in order");

    // Space reappears: the next emission must arrive (the batch
    // buffer survived the full-channel stretch).
    tick();
    wait_steps(&mut steps, 71).await;
    let recovered = drain_all(&mut rx).await;
    assert_eq!(recovered.len(), 1, "the post-full emission arrives");
    assert_eq!(
        recovered[0][0].payload.as_ref(),
        71u64.to_le_bytes(),
        "and it is tick 71's snapshot"
    );

    control
        .send(RoomControl::Shutdown)
        .await
        .expect("control alive");
    tick();
    tokio::time::timeout(Duration::from_secs(2), handle)
        .await
        .expect("room did not shut down")
        .expect("room task panicked");
}

#[tokio::test]
async fn unchanged_group_is_silent_until_keepalive() {
    // Keep-alive every 3 steps (10 Hz under a 30 Hz room).
    let (step_tx, mut steps) = mpsc::channel(64);
    let mut room = GLRoom::new(
        RoomConfig {
            id: RoomId(2),
            keepalive_hz: 10.0,
            ..Default::default()
        },
        GroupLogic {
            player_entity: HashMap::new(),
            next: 0,
            dirty: std::collections::HashSet::new(),
            step_no: 0,
            steps: step_tx,
        },
    );

    let (ent, mut a_rx) = room.join(ConnectionId(1)).await;
    wait_steps(&mut steps, 1).await;
    // Step 1 (the join tick): the membership change shipped a snapshot.
    let first = next_batch_full(&mut a_rx).await;
    assert_eq!(
        batch_frames(&first),
        vec![(0x7010, ent.to_le_bytes().to_vec())]
    );

    // Steps 2..8: no change. The room stays silent — except on the
    // keep-alive steps (3 and 6), which re-send the cached snapshot.
    for _ in 0..7 {
        room.step().await;
    }
    wait_steps(&mut steps, 8).await;

    let mut got = vec![first];
    while let Ok(batch) = a_rx.try_recv() {
        got.push(batch);
    }
    assert_eq!(
        got.len(),
        3,
        "one emission (step 1) + two keep-alive re-sends (steps 3, 6), \
         nothing else"
    );
    for batch in &got {
        assert_eq!(
            batch_frames(batch),
            vec![(0x7010, ent.to_le_bytes().to_vec())],
            "keep-alive re-sends the cached snapshot bytes"
        );
    }

    room.shutdown().await;
}

// -----------------------------------------------------------------
// F5: keep-alive rate above the room rate. The registry rejects such
// a config at room creation; direct construction (library use) must
// still never be silent: the constructor warns once naming both
// rates, and the cadence clamps to every step (the observable
// behavior: an unchanged group re-sends on *every* step).
// -----------------------------------------------------------------

#[tokio::test]
async fn keepalive_above_tick_warns_at_construction_and_clamps_to_every_step() {
    // Thread-local subscriber (NOT the process-global default: other
    // tests in this binary may run concurrently on other threads, and
    // the never-emitted test owns the global slot).
    let (warn_tx, mut warns) = mpsc::channel::<String>(64);
    let (tick_tx, tick_rx) = broadcast::channel(64);
    let (control, control_rx) = channel(64);
    let (step_tx, mut steps) = mpsc::channel(64);

    // Direct construction with keepalive_hz = 60 under a 30 Hz room —
    // the misconfiguration the registry would have rejected.
    let actor = tracing::subscriber::with_default(WarnCapture { tx: warn_tx.clone() }, || {
        RoomActor::new(
            RoomConfig {
                id: RoomId(25),
                keepalive_hz: 60.0,
                ..Default::default()
            },
            (),
            Box::new(GroupLogic {
                player_entity: HashMap::new(),
                next: 0,
                dirty: std::collections::HashSet::new(),
                step_no: 0,
                steps: step_tx,
            }),
            tick_rx,
            control_rx,
            1,
            null_metrics_tx(),
            None,
        )
    });
    let handle = tokio::spawn(actor.run());
    let t0 = Instant::now();

    // The construction-time warn fired exactly once and names both
    // rates (synchronous: the warn! runs inside the constructor).
    let w = warns.try_recv().expect("misconfigured construction must warn");
    assert!(
        w.contains("keepalive_hz=60") && w.contains("tick_hz=30"),
        "warn must name both rates: {w}"
    );
    assert!(
        warns.try_recv().is_err(),
        "the construction warn must fire exactly once: {w}"
    );

    // Clamped behavior: the join's own emission, then EVERY step is a
    // keep-alive step (interval 1) re-sending the cached snapshot.
    let (out_tx, mut a_rx) = mpsc::channel::<FrameBatch>(64);
    let (reply_tx, reply_rx) =
        oneshot::channel::<Result<(EntityId, Mailbox<Action>), CoreError>>();
    control
        .send(RoomControl::Join {
            conn: ConnectionId(26),
            out: out_tx,
            reply: reply_tx,
        })
        .await
        .expect("control alive");
    for n in 1..=6u64 {
        tick_tx
            .send(TickInfo {
                tick: n,
                at: t0 + Duration::from_secs_f64(n as f64 / 30.0),
            })
            .expect("room subscriber alive");
    }
    let _ = reply_rx
        .await
        .expect("join reply dropped")
        .expect("join accepted (room not full)");
    wait_steps(&mut steps, 6).await;

    let got = drain_all(&mut a_rx).await;
    assert_eq!(
        got.len(),
        6,
        "join emission + 5 keep-alive re-sends (one per step, no \
         silence at all): {got:?}"
    );
    for (i, batch) in got.iter().enumerate() {
        assert_eq!(
            batch_frames(batch),
            vec![(0x7010, 1u64.to_le_bytes().to_vec())],
            "step {i} must carry the cached snapshot (entity 1)"
        );
    }

    drop(tick_tx); // ticker closed → the room exits cleanly
    tokio::time::timeout(Duration::from_secs(2), handle)
        .await
        .expect("room did not exit on closed ticker")
        .expect("room task panicked");

    // No false positives: legitimate ratios must not warn. keep-alive
    // == tick is exactly "one per step" (as configured); 1 Hz is the
    // default setup.
    for keepalive in [30.0, 1.0] {
        let (_tick2, tick_rx2) = broadcast::channel(8);
        let (_control2, control_rx2) = channel(8);
        let (step2_tx, _step2_rx) = mpsc::channel::<u64>(8);
        tracing::subscriber::with_default(WarnCapture { tx: warn_tx.clone() }, || {
            let _actor2 = RoomActor::new(
                RoomConfig {
                    id: RoomId(27),
                    keepalive_hz: keepalive,
                    ..Default::default()
                },
                (),
                Box::new(GroupLogic {
                    player_entity: HashMap::new(),
                    next: 0,
                    dirty: std::collections::HashSet::new(),
                    step_no: 0,
                    steps: step2_tx,
                }),
                tick_rx2,
                control_rx2,
                1,
                null_metrics_tx(),
            None,
            );
        });
    }
    assert!(
        warns.try_recv().is_err(),
        "keepalive_hz <= tick_hz must not warn"
    );
}

// -----------------------------------------------------------------
// F4 diagnostic: a group that has members but has never emitted
// (snapshot → false on its first tick, although a fresh group's first
// tick is a membership change and must emit) must be warned about —
// exactly once, naming the group. The diagnostic previously had no
// test; a violating logic (e.g. the shared-ledger misuse the room
// cannot distinguish from legitimate silence) stays invisible without
// one.
// -----------------------------------------------------------------

/// Contract-violating logic: `snapshot` returns `false` on *every*
/// tick, including a fresh group's first tick.
struct SilentLogic;

impl GameLogic<()> for SilentLogic {
    type GroupKey = PlayerId;
    type Strip = ();

    fn snapshot_op(&self) -> u16 {
        0x7030
    }
    fn private_op(&self) -> u16 {
        0x7031
    }

    fn group_of(&self, _w: &(), player: PlayerId) -> Self::GroupKey {
        player
    }

    fn snapshot(
        &mut self,
        _w: &mut (),
        _c: &TickCtx,
        _g: &Self::GroupKey,
        _borrowed: &[crate::shard::BorderRecord<()>],
        _o: &mut bytes::BytesMut,
    ) -> bool {
        false // even the first tick: a contract violation
    }

    fn on_join(&mut self, _w: &mut (), conn: ConnectionId) -> Admission {
        Admission {
            player: PlayerId(conn.0),
            entity: 1,
        }
    }
    fn on_leave(&mut self, _w: &mut (), _player: PlayerId) {}
    fn ingest(&mut self, _w: &mut (), _c: &TickCtx, actions: &mut Vec<Action>) {
        actions.clear();
    }
    fn update(&mut self, _w: &mut (), _c: &TickCtx) {}
}

// Faz 1 trait split: shared contract on `GameLogic`; no room-exclusive
// hook used (empty `RoomLogic` impl).
impl RoomLogic<()> for SilentLogic {}

/// Lock-free WARN-capturing subscriber: events are pushed over an mpsc
/// channel (never blocking); no shared state to protect. The `warn!`
/// macro carries its text in a `message` field, so the field list is
/// the log line.
struct WarnCapture {
    tx: mpsc::Sender<String>,
}

impl tracing::Subscriber for WarnCapture {
    fn enabled(&self, meta: &tracing::Metadata<'_>) -> bool {
        *meta.level() == tracing::Level::WARN
    }

    fn new_span(&self, _span: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        // The room code creates no spans; placeholder never used.
        tracing::span::Id::from_non_zero_u64(std::num::NonZeroU64::MIN)
    }

    fn record(&self, _span: &tracing::span::Id, _values: &tracing::span::Record<'_>) {}

    fn record_follows_from(&self, _span: &tracing::span::Id, _follows: &tracing::span::Id) {}

    fn enter(&self, _span: &tracing::span::Id) {}

    fn exit(&self, _span: &tracing::span::Id) {}

    fn event(&self, event: &tracing::Event<'_>) {
        let mut fields: Vec<(String, String)> = Vec::new();
        event.record(&mut WarnFieldSink {
            fields: &mut fields,
        });
        let line = fields
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join(" ");
        let _ = self.tx.try_send(line);
    }
}

struct WarnFieldSink<'a> {
    fields: &'a mut Vec<(String, String)>,
}

impl tracing::field::Visit for WarnFieldSink<'_> {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        self.fields
            .push((field.name().to_string(), format!("{value:?}")));
    }
}

#[tokio::test]
async fn never_emitted_group_warns_once_naming_the_group() {
    // Only this test sets the process-global default; the other tests
    // in this binary neither set it nor assert on logging.
    let (warn_tx, mut warns) = mpsc::channel::<String>(64);
    tracing::subscriber::set_global_default(WarnCapture { tx: warn_tx })
        .expect("only this test sets the global default");

    let (tick_tx, tick_rx) = broadcast::channel(64);
    let (control, control_rx) = channel(16);
    let actor = RoomActor::new(
        RoomConfig {
            id: RoomId(9),
            ..Default::default()
        },
        (),
        Box::new(SilentLogic),
        tick_rx,
        control_rx,
        1,
        null_metrics_tx(),
            None,
    );
    let handle = tokio::spawn(actor.run());
    let t0 = Instant::now();

    // Tick 1: the join is processed, the group is created (Vacant —
    // no check yet) and its first `snapshot()` returns `false`.
    let (out_tx, _out_rx) = mpsc::channel::<FrameBatch>(8);
    let (reply_tx, reply_rx) =
        oneshot::channel::<Result<(EntityId, Mailbox<Action>), CoreError>>();
    control
        .send(RoomControl::Join {
            conn: ConnectionId(42),
            out: out_tx,
            reply: reply_tx,
        })
        .await
        .expect("control alive");
    tick_tx
        .send(TickInfo {
            tick: 1,
            at: t0 + Duration::from_secs_f64(1.0 / 30.0),
        })
        .expect("room subscriber alive");
    let _ = reply_rx
        .await
        .expect("join reply dropped")
        .expect("join accepted (room not full)");
    tokio::time::sleep(Duration::from_millis(20)).await;
    // No diagnostic for THIS group on its own first tick (the check
    // only sees a group that existed on the previous tick). Other
    // tests in this binary share the global default and may emit
    // their own warns (e.g. RecLogic, which never emits) — only lines
    // naming our group are ours.
    while let Ok(line) = warns.try_recv() {
        assert!(
            !line.contains("PlayerId(42)"),
            "no diagnostic on the group's own first tick: {line}"
        );
    }

    // Ticks 2..=4: the group is Occupied with `last = None` — the
    // diagnostic must fire on tick 2 and (flag) not repeat.
    for n in 2u64..=4 {
        tick_tx
            .send(TickInfo {
                tick: n,
                at: t0 + Duration::from_secs_f64(n as f64 / 30.0),
            })
            .expect("room subscriber alive");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    drop(tick_tx); // ticker closed → the room exits cleanly
    tokio::time::timeout(Duration::from_secs(2), handle)
        .await
        .expect("room did not exit on closed ticker")
        .expect("room task panicked");

    let mut mine = Vec::new();
    while let Ok(line) = warns.try_recv() {
        if line.contains("PlayerId(42)") {
            mine.push(line);
        }
    }
    assert_eq!(mine.len(), 1, "warn fires exactly once: {mine:?}");
    assert!(
        mine[0].contains("group_key=PlayerId(42)"),
        "warn must name the group: {}",
        mine[0]
    );
    assert!(
        mine[0].contains("members=1"),
        "warn must report the member count: {}",
        mine[0]
    );
}

/// Receive one batch with a timeout (the positive-side barrier: the
/// room flushed this connection, so its step's fan-out has reached it).
async fn next_batch_full(rx: &mut mpsc::Receiver<FrameBatch>) -> Vec<FrameBody> {
    tokio::time::timeout(Duration::from_secs(2), rx.recv())
        .await
        .expect("timed out waiting for a batch")
        .expect("out channel closed")
}

/// Take everything currently queued (after `wait_steps`, the fan-out of
/// every reported step has completed).
async fn drain_all(rx: &mut mpsc::Receiver<FrameBatch>) -> Vec<Vec<FrameBody>> {
    let mut out = Vec::new();
    while let Ok(batch) = rx.try_recv() {
        out.push(batch);
    }
    out
}

// ── capacity + fairness guardrails (behaviour lock) ──────────────

/// Capacity guardrail: at `max_players` the next join is rejected with
/// `CoreError::RoomFull` — no entity, no action channel, no room
/// state — while the room (and its members) keeps working.
#[tokio::test]
async fn join_rejected_when_room_is_full() {
    let (dt_tx, _dts) = mpsc::channel(16);
    let (op_tx, mut ops) = mpsc::channel(16);
    let mut h = Harness::new(
        1,
        RoomConfig {
            id: RoomId(1),
            max_players: Some(2),
            ..Default::default()
        },
        RecLogic {
            dts: dt_tx,
            ops: op_tx,
        },
    );
    let period = Duration::from_secs_f64(1.0 / 30.0);

    // Two joins fill the room.
    let (_e1, _a1) = h.join(ConnectionId(1), 1).await;
    let (_e2, _a2) = h.join(ConnectionId(2), 1).await;

    // The third join is structurally rejected (the reply carries the
    // error; nothing is recorded in the room).
    let (out3_tx, _out3_rx) = mpsc::channel::<FrameBatch>(8);
    let (reply3_tx, reply3_rx) =
        oneshot::channel::<Result<(EntityId, Mailbox<Action>), CoreError>>();
    h.control
        .send(RoomControl::Join {
            conn: ConnectionId(3),
            out: out3_tx,
            reply: reply3_tx,
        })
        .await
        .expect("control alive");
    for _ in 0..2 {
        h.tick(period);
    }
    match tokio::time::timeout(Duration::from_secs(2), reply3_rx)
        .await
        .expect("timed out waiting for the rejection")
        .expect("reply dropped")
    {
        Err(CoreError::RoomFull(id)) => {
            assert_eq!(id, 1, "the error names the rejecting room")
        }
        other => panic!("expected RoomFull, got {other:?}"),
    }

    // Existing members are unaffected: the surviving member's action
    // still reaches the ingest on the next step.
    _a1
        .send(Action {
            conn: ConnectionId(1),
            player: PlayerId(1),
            op: 0x1500,
            payload: bytes::Bytes::new(),
        })
        .await
        .expect("member's action channel alive");
    h.tick(period);
    let op = tokio::time::timeout(Duration::from_secs(2), ops.recv())
        .await
        .expect("timed out waiting for the member op")
        .expect("ops closed");
    assert_eq!(op, 0x1500, "the surviving member's action is still ingested");
    h.shutdown().await;
}

/// Fairness guardrail: a connection that floods its own action
/// channel can no longer evict ANYONE ELSE's actions. The READ phase
/// is a bounded pull (per-connection budget + room pull budget), not
/// a merged list with an oldest-drop: the victim's one action per tick
/// is ingested on every tick, and the flooder's excess stays in its
/// OWN channel (deferred; the room drops nothing).
///
/// Under the old semantics (merged list, `drain(..over)` = oldest) the
/// merged list is built in `conns` iteration order and the overflow
/// drops its HEAD: with one flooded connection and one quiet one, the
/// quiet connection's actions sit in a small contiguous block of the
/// list, and the flooder's backlog determines which block overflows —
/// i.e. a single flooder could evict the other connection's actions
/// (which block was dropped depended on the hash order, so even the
/// victim was arbitrary). The per-connection pull budget removes the
/// interaction entirely: every connection's ingest is bounded by its
/// own budget, whoever it is.
#[tokio::test]
async fn flooder_cannot_evict_other_connections_actions() {
    let (dt_tx, _dts) = mpsc::channel(64);
    // Wide: the room ingests 188 ops (20 victim + 168 flood) and the
    // logic forwards each via try_send — the observation channel must
    // not be the thing that overflows in this test.
    let (op_tx, mut ops) = mpsc::channel(512);
    let mut h = Harness::new(
        1,
        RoomConfig {
            id: RoomId(1),
            // The fairness probe: a tight pull budget and a tight
            // per-connection budget (the old code had neither; the
            // merged list grew with the flooder's backlog).
            max_pending_actions: 16,
            max_actions_per_conn_per_tick: 8,
            ..Default::default()
        },
        RecLogic {
            dts: dt_tx,
            ops: op_tx,
        },
    );
    let period = Duration::from_secs_f64(1.0 / 30.0);

    // The quiet connection ("victim") and the flooded connection
    // ("flooder"); which block of the old merged list overflowed
    // depended on the HashMap iteration order, so neither role is
    // special — the new per-connection budgets make it a non-issue.
    let (_ev, victim) = h.join(ConnectionId(1), 1).await;
    let (_ef, flood) = h.join(ConnectionId(2), 1).await;

    // The flooder fills its own action channel to capacity (256):
    // the flood backlog the room would have merged (and overflowed)
    // under the old READ.
    let mut stuffed = 0usize;
    while let Ok(()) = flood.try_send(Action {
        conn: ConnectionId(2),
        player: PlayerId(2),
        op: 0x3000,
        payload: bytes::Bytes::new(),
    }) {
        stuffed += 1;
    }
    assert_eq!(stuffed, 256, "the flood backlog is the channel capacity");

    // 20 ticks: the victim sends exactly one action per tick.
    for t in 0..20u16 {
        victim
            .try_send(Action {
                conn: ConnectionId(1),
                player: PlayerId(1),
                op: 0x2000 + t,
                payload: bytes::Bytes::new(),
            })
            .expect("victim channel never full (one op per tick)");
        h.tick(period);
    }
    // One more tick so the last queued op is pulled and ingested.
    h.tick(period);

    // Collect everything ingested (the ticks are fire-and-forget, so
    // wait until the full expected volume has landed: 20 victim ops +
    // 8 floods/tick × 21 ticks = 168). Ingest ORDER between the two
    // connections is HashMap-driven and not asserted; OWNERSHIP is
    // what the guardrail guarantees.
    let mut victim_ops = 0u32;
    let mut flood_ops = 0u32;
    let mut total = 0u32;
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while total < 188 && std::time::Instant::now() < deadline {
        match ops.try_recv() {
            Ok(op) => {
                total += 1;
                if (0x2000..0x2014).contains(&op) {
                    victim_ops += 1;
                } else if op == 0x3000 {
                    flood_ops += 1;
                }
            }
            Err(_) => tokio::time::sleep(Duration::from_millis(5)).await,
        }
    }
    assert_eq!(
        total,
        188,
        "the room ingested the full expected volume (20 + 168)"
    );
    assert_eq!(
        victim_ops,
        20,
        "EVERY victim op was ingested — the flood evicted none of them"
    );
    assert_eq!(
        flood_ops,
        168,
        "the room pulled exactly 8 flood ops per tick (its per-conn budget)"
    );
    // The flooder's excess was DEFERRED in its own channel: the room
    // pulled 8/tick × 21 ticks = 168 (the ingested volume above), so
    // 88 of the original 256 are still queued — the room dropped
    // nothing. Observe it by filling the free slots: exactly 168
    // sends fit (= the amount pulled), the 169th hits Full.
    let mut free = 0usize;
    // (Full ends the loop: the backlog is exactly 256 − free.)
    while let Ok(()) = flood.try_send(Action {
        conn: ConnectionId(2),
        player: PlayerId(2),
        op: 0x3001,
        payload: bytes::Bytes::new(),
    }) {
        free += 1;
    }
    assert_eq!(
        free,
        8 * 21,
        "exactly the per-tick pull (8/tick × 21 ticks) freed slots; \
         the rest is still deferred in the flooder's own channel"
    );
    h.shutdown().await;
}
