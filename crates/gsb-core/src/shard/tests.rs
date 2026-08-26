//! Unit tests for [`super`] (moved out of the module file so the
//! implementation reads on its own; still a child module, so
//! `use super::*` reaches the parent's private items exactly as
//! before).

//! The shard protocol tests (the required invariants of the sharding
//! round, plus the race gates):
//!
//! - migration never drops or duplicates an entity — including a
//!   ping-pong across the boundary (checked per tick index);
//! - a migrated entity keeps its wire id, and two shards' id spaces
//!   are disjoint (no cross-shard collision);
//! - in-flight input survives the migration (the action channel moves
//!   with the connection and is applied by the receiving shard);
//! - the leave/migration race is deterministic: a `Migrate` whose join
//!   is already dead (the leave was processed first) is rejected by
//!   the epoch gate, and a fresh join (newer epoch) is still accepted;
//! - boundary visibility: each shard's snapshot includes the neighbor's
//!   boundary entities (the borrowed set).
//!
//! The harness drives two [`ShardActor`]s off a MANUAL ticker (a
//! broadcast sender the test feeds tick by tick), so the ordering of
//! the shards' tick processing is deterministic and the exactly-once
//! invariant can be checked per tick index.

use super::*;
use crate::room::Admission;
use std::time::Duration;

use crate::channel::channel;

/// The strip payload these protocol tests use: exactly what the old
/// core-fixed record carried (identity + truncated position), so
/// every assertion keeps its pre-generalization meaning while the
/// envelope becomes the generic [`BorderRecord`] around a
/// game-owned payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TStrip {
    x: i32,
    y: i32,
}

/// Shorthand for building one expected/actual border record.
fn rec(wire: u64, x: i32, y: i32) -> BorderRecord<TStrip> {
    BorderRecord {
        wire,
        state: TStrip { x, y },
    }
}

/// The test world: wire → (x, y, mode); `mode` ∈ {-1, 0, +1} = the
/// per-tick x step. The map is [-10, 10): shard 0 owns x < 0, shard 1
/// owns x >= 0 (one boundary at x = 0 — the smallest topology).
#[derive(Default, Debug, Clone)]
struct TWorld {
    ents: HashMap<u64, (f32, f32, i8)>,
}

/// The migration state (the demo's shape).
#[derive(Debug, Clone)]
struct TState {
    x: f32,
    y: f32,
    mode: i8,
}

/// An observation the test logic hands the harness over its (bounded,
/// polled) channel — the test never reaches into a shard's world
/// directly (no shared state, no locks).
#[derive(Debug)]
enum Obs {
    /// This shard's world content after its phase 3 (SYSTEMS):
    /// (shard, tick, [(wire, x, y, mode)]).
    Content(usize, u64, Vec<(u64, f32, f32, i8)>),
    /// This shard reported a wire as migrating out (phase 4):
    /// (shard, tick, wire).
    Migrate(usize, u64, u64),
}

/// The test shard logic: two shards over one map (see `TWorld`), one
/// snapshot group, deterministic movement (a targeted entity steps
/// `mode` in x per tick), wire ids from the range partitioning.
struct TLogic {
    index: usize,
    next_serial: u64,
    player_ent: HashMap<PlayerId, u64>,
    ent_player: HashMap<u64, PlayerId>,
    last_tick: u64,
    obs: mpsc::Sender<Obs>,
    ops: mpsc::Sender<(PlayerId, u16)>,
}

impl TLogic {
    fn region_of(x: f32) -> usize {
        if x < 0.0 {
            0
        } else {
            1
        }
    }
}

// Faz 1 trait split: the shared contract (snapshot groups, tick seam,
// membership) implements the `GameLogic` supertrait; the sharding seam
// stays on `ShardLogic`.
impl GameLogic<TWorld> for TLogic {
    type GroupKey = ();
    type Strip = TStrip;

    fn snapshot_op(&self) -> u16 {
        0x7100
    }
    fn private_op(&self) -> u16 {
        0x7101
    }
    fn group_of(&self, _w: &TWorld, _p: PlayerId) -> Self::GroupKey {}
    fn snapshot(
        &mut self,
        w: &mut TWorld,
        _ctx: &TickCtx,
        _g: &Self::GroupKey,
        borrowed: &[BorderRecord<TStrip>],
        out: &mut bytes::BytesMut,
    ) -> bool {
        // The test's wire format: own entities + the borrowed boundary
        // set, sorted by wire, 16 bytes per record (u64 wire LE,
        // i32 x LE, i32 y LE) — the harness parses it back from the
        // connection's out channel (test 5).
        let mut recs: Vec<(u64, i32, i32)> = w
            .ents
            .iter()
            .map(|(wire, (x, y, _))| (*wire, *x as i32, *y as i32))
            .collect();
        for b in borrowed {
            recs.push((b.wire, b.state.x, b.state.y));
        }
        recs.sort_unstable_by_key(|r| r.0);
        for (wire, x, y) in &recs {
            out.extend_from_slice(&wire.to_le_bytes());
            out.extend_from_slice(&x.to_le_bytes());
            out.extend_from_slice(&y.to_le_bytes());
        }
        true
    }
    fn on_join(&mut self, w: &mut TWorld, conn: ConnectionId) -> Admission {
        // Deterministic spawn: x = (conn.0 % 20) - 10 (conn 1 → -9 in
        // shard 0; conn 10 → 0 in shard 1; conn 11 → +1 in shard 1),
        // y = 0, no motion.
        let x = (conn.0 % 20) as f32 - 10.0;
        self.next_serial += 1;
        let wire = self.serial_base() + self.next_serial;
        w.ents.insert(wire, (x, 0.0, 0));
        // Test identity policy: the conn id doubles as the player id.
        let player = PlayerId(conn.0);
        self.player_ent.insert(player, wire);
        self.ent_player.insert(wire, player);
        Admission {
            player,
            entity: wire,
        }
    }
    fn on_leave(&mut self, w: &mut TWorld, player: PlayerId) {
        if let Some(wire) = self.player_ent.remove(&player) {
            self.ent_player.remove(&wire);
            w.ents.remove(&wire);
        }
    }
    fn ingest(&mut self, w: &mut TWorld, _ctx: &TickCtx, actions: &mut Vec<Action>) {
        for a in actions.drain(..) {
            let _ = self.ops.try_send((a.player, a.op));
            // The test's ops: 1000 = step +1/tick, 1001 = step -1/tick,
            // 1002 = stop.
            let mode = match a.op {
                1000 => 1,
                1001 => -1,
                _ => 0,
            };
            if let Some(wire) = self.player_ent.get(&a.player).copied()
                && let Some(e) = w.ents.get_mut(&wire)
            {
                e.2 = mode;
            }
        }
    }
    fn update(&mut self, w: &mut TWorld, ctx: &TickCtx) {
        self.last_tick = ctx.tick;
        let mut steps: Vec<(u64, f32, f32, i8)> = Vec::new();
        for (&wire, (x, y, mode)) in &w.ents {
            if *mode != 0 {
                steps.push((wire, x + *mode as f32, *y, *mode));
            }
        }
        for (wire, x, y, mode) in steps {
            if let Some(e) = w.ents.get_mut(&wire) {
                *e = (x, y, mode);
            }
        }
        // The observation: the world content AFTER this tick's step
        // (phase 3 — before the phase-4 migration bookkeeping, which
        // is accounted for by the harness's "reported out last tick"
        // rule — see `owners_at`).
        let mut c: Vec<(u64, f32, f32, i8)> =
            w.ents.iter().map(|(wire, e)| (*wire, e.0, e.1, e.2)).collect();
        c.sort_unstable_by_key(|e| e.0);
        let _ = self.obs.try_send(Obs::Content(self.index, ctx.tick, c));
    }
}

impl ShardLogic<TWorld> for TLogic {
    type State = TState;

    fn index(&self) -> usize {
        self.index
    }
    fn shard_count(&self) -> usize {
        2
    }
    fn serial_base(&self) -> u64 {
        self.index as u64 * SHARD_SERIAL_RANGE
    }
    fn serial_range(&self) -> u64 {
        SHARD_SERIAL_RANGE
    }
    fn serial_used(&self) -> u64 {
        self.next_serial
    }
    fn neighbors(&self) -> &[usize] {
        if self.index == 0 {
            &[1]
        } else {
            &[0]
        }
    }
    fn collect_migrations(&mut self, w: &mut TWorld, neighbor: usize) -> Vec<Migrating<TState>> {
        let mut out = Vec::new();
        for (&wire, (x, y, mode)) in &w.ents {
            if TLogic::region_of(*x) == neighbor {
                let _ = self.obs.try_send(Obs::Migrate(self.index, self.last_tick, wire));
                out.push(Migrating {
                    wire,
                    state: TState {
                        x: *x,
                        y: *y,
                        mode: *mode,
                    },
                    player: self.ent_player.get(&wire).copied(),
                });
            }
        }
        out
    }
    fn on_migrate_in(
        &mut self,
        w: &mut TWorld,
        wire: u64,
        state: TState,
        player: Option<PlayerId>,
    ) {
        w.ents.insert(wire, (state.x, state.y, state.mode));
        if let Some(p) = player {
            self.player_ent.insert(p, wire);
            self.ent_player.insert(wire, p);
        }
    }
    fn on_migrate_out(&mut self, w: &mut TWorld, wire: u64) {
        if let Some(p) = self.ent_player.remove(&wire) {
            self.player_ent.remove(&p);
        }
        w.ents.remove(&wire);
    }
    fn collect_border(&self, w: &TWorld) -> Vec<BorderRecord<TStrip>> {
        // Border = entities within 1 unit of the region edge (x = 0).
        w.ents
            .iter()
            .filter(|(_, (x, _, _))| x.abs() <= 1.0)
            .map(|(wire, (x, y, _))| rec(*wire, *x as i32, *y as i32))
            .collect()
    }
    fn own_wires(&self, w: &TWorld) -> Vec<u64> {
        w.ents.keys().copied().collect()
    }
}

/// The harness: two shards off a manual ticker; the test feeds ticks
/// and observes each shard's world content (and the migration
/// reports) through the obs channel.
struct Harness {
    tick_tx: broadcast::Sender<TickInfo>,
    shard_txs: [Mailbox<ShardMsg<TState, TStrip>>; 2],
    obs: mpsc::Receiver<Obs>,
    ops: mpsc::Receiver<(PlayerId, u16)>,
    #[allow(dead_code)]
    handles: Vec<tokio::task::JoinHandle<()>>,
    t: u64,
    /// Migration reports: (tick, from-shard) → wires.
    migrated: HashMap<(u64, usize), Vec<u64>>,
    /// The latest completed tick's content, per shard.
    content: [Vec<(u64, f32, f32, i8)>; 2],
}

fn metrics_null() -> mpsc::Sender<MetricsEvent> {
    let (tx, _rx) = mpsc::channel(1);
    tx
}

impl Harness {
    fn new() -> Self {
        let (tick_tx, _) = broadcast::channel(64);
        let (tx0, rx0) = channel::<ShardMsg<TState, TStrip>>(128);
        let (tx1, rx1) = channel::<ShardMsg<TState, TStrip>>(128);
        let (obs_tx, obs_rx) = mpsc::channel(4096);
        let (ops_tx, ops_rx) = mpsc::channel(4096);
        let (dummy_tx, _dummy_rx) = channel::<ShardMsg<TState, TStrip>>(1);
        let cfg = RoomConfig {
            id: RoomId(7),
            keepalive_hz: 0.0, // silence the keep-alive re-sends
            metrics_cadence_hz: 0.0,
            ..Default::default()
        };
        let h0 = tokio::spawn(
            ShardActor::new(
                cfg.clone(),
                0,
                TWorld::default(),
                Box::new(TLogic {
                    index: 0,
                    next_serial: 0,
                    player_ent: HashMap::new(),
                    ent_player: HashMap::new(),
                    last_tick: 0,
                    obs: obs_tx.clone(),
                    ops: ops_tx.clone(),
                }),
                tick_tx.subscribe(),
                rx0,
                // Indexed by the receiver's shard index (the actor's
                // lookup); slot 0 (itself) is the dummy.
                vec![dummy_tx.clone(), tx1.clone()],
                1,
                metrics_null(),
                None, // no result sink in the protocol harness
            )
            .run(),
        );
        let h1 = tokio::spawn(
            ShardActor::new(
                cfg,
                1,
                TWorld::default(),
                Box::new(TLogic {
                    index: 1,
                    next_serial: 0,
                    player_ent: HashMap::new(),
                    ent_player: HashMap::new(),
                    last_tick: 0,
                    obs: obs_tx.clone(),
                    ops: ops_tx,
                }),
                tick_tx.subscribe(),
                rx1,
                vec![tx0.clone(), dummy_tx],
                1,
                metrics_null(),
                None, // no result sink in the protocol harness
            )
            .run(),
        );
        drop(obs_tx);
        Harness {
            tick_tx,
            shard_txs: [tx0, tx1],
            obs: obs_rx,
            ops: ops_rx,
            handles: vec![h0, h1],
            t: 0,
            migrated: HashMap::new(),
            content: [Vec::new(), Vec::new()],
        }
    }

    /// Feed one tick to the manual ticker and wait until BOTH shards
    /// have reported their content for it (the obs channel carries the
    /// per-shard, per-tick observations; a tick is "done" when both
    /// shards reported it). Returns the two shards' content.
    async fn tick(&mut self) -> [Vec<(u64, f32, f32, i8)>; 2] {
        self.t += 1;
        let t = self.t;
        self.tick_tx
            .send(TickInfo {
                tick: t,
                at: Instant::now(),
            })
            .expect("at least one subscriber");
        let mut content: [Vec<(u64, f32, f32, i8)>; 2] = [Vec::new(), Vec::new()];
        // Track which shards have REPORTED this tick (an empty shard
        // reports an empty vec, so emptiness is not a done-signal).
        let mut reported = [false; 2];
        let deadline = Instant::now() + Duration::from_secs(5);
        while !reported.iter().all(|&r| r) {
            let wait = deadline.saturating_duration_since(Instant::now());
            let item = match tokio::time::timeout(wait, self.obs.recv()).await {
                Ok(x) => x.expect("obs channel closed"),
                Err(_) => {
                    panic!(
                        "shards did not both process tick {} in time; \
                         reported={reported:?} got={content:?}",
                        t
                    );
                }
            };
            match item {
                Obs::Content(s, tick, c) if tick == t && !reported[s] => {
                    content[s] = c;
                    reported[s] = true;
                }
                Obs::Migrate(s, tick, wire) => {
                    self.migrated.entry((tick, s)).or_default().push(wire);
                }
                _ => {}
            }
        }
        self.content = content;
        self.content.clone()
    }

    /// Join `conn` to `shard` (the registry's home-shard routing is
    /// the server's business; the test picks the shard directly).
    /// The join is processed on the next tick; returns (wire, the
    /// action channel, the snapshot out channel).
    async fn join(
        &mut self,
        shard: usize,
        conn: ConnectionId,
        epoch: u64,
    ) -> (u64, mpsc::Sender<Action>, mpsc::Receiver<FrameBatch>) {
        let (reply_tx, reply_rx) = oneshot::channel();
        let (out_tx, out_rx) = mpsc::channel(64);
        self.shard_txs[shard]
            .send(ShardMsg::Join {
                conn,
                epoch,
                out: out_tx,
                reply: reply_tx,
            })
            .await
            .expect("shard channel open");
        // Drive the tick that processes the join (its CONTROL phase
        // sends the reply), then collect the reply. In the test logic
        // the entity id IS the wire id.
        let _ = self.tick().await;
        let (wire, actions) = reply_rx
            .await
            .expect("join reply delivered")
            .expect("join succeeded");
        (wire, actions, out_rx)
    }

    /// Send one input action (the connection actor's role).
    async fn act(&self, actions: &mpsc::Sender<Action>, conn: ConnectionId, op: u16) {
        actions
            .send(Action {
                conn,
                // Test identity policy (matches TLogic::on_join): the
                // conn id doubles as the player id.
                player: PlayerId(conn.0),
                op,
                payload: bytes::Bytes::new(),
            })
            .await
            .expect("action channel open");
    }

    /// The registry's leave broadcast: to ALL shards (exactly one owns
    /// the connection; the others no-op on the entity-id guard).
    async fn leave(&self, conn: ConnectionId, entity: EntityId, epoch: u64) {
        for shard in 0..2 {
            self.shard_txs[shard]
                .send(ShardMsg::Leave {
                    conn,
                    entity,
                    epoch,
                })
                .await
                .expect("shard channel open");
        }
    }

    /// The ops the shards ingested, in order (drain).
    async fn ops_drained(&mut self) -> Vec<(PlayerId, u16)> {
        let mut out = Vec::new();
        while let Ok(op) = self.ops.try_recv() {
            out.push(op);
        }
        out
    }
}

// -----------------------------------------------------------------
// Table-pruning locks: the connection tables must not grow forever
// with connection churn (`conn_epoch` pruned on Leave; tombstones
// TTL'd + swept). These drive a BARE actor (not spawned): the
// harness above can only see observable wire behavior — the right
// level for the protocol invariants, but too coarse for "is this
// exact map entry gone". A child module may touch the private
// tables directly; the assertions below are the behavior locks.
// -----------------------------------------------------------------

/// An unspawned shard actor for the table tests above/below.
fn bare_shard(index: usize) -> ShardActor<TWorld, (), TState, TStrip> {
    let (_tick_tx, tick_rx) = broadcast::channel(64);
    let (_self_tx, rx) = channel::<ShardMsg<TState, TStrip>>(16);
    // Two dummy neighbor slots (TLogic::neighbors targets 0 and 1).
    let (n0, _n0_rx) = channel::<ShardMsg<TState, TStrip>>(8);
    let (n1, _n1_rx) = channel::<ShardMsg<TState, TStrip>>(8);
    let (obs, _obs_rx) = mpsc::channel(16);
    let (ops, _ops_rx) = mpsc::channel(16);
    ShardActor::new(
        RoomConfig {
            id: RoomId(9),
            keepalive_hz: 0.0,
            metrics_cadence_hz: 0.0,
            ..Default::default()
        },
        index,
        TWorld::default(),
        Box::new(TLogic {
            index,
            next_serial: 0,
            player_ent: HashMap::new(),
            ent_player: HashMap::new(),
            last_tick: 0,
            obs,
            ops,
        }),
        tick_rx,
        rx,
        vec![n0, n1],
        1,
        metrics_null(),
        None, // no result sink
    )
}

fn tctx(tick: u64) -> TickCtx {
    TickCtx {
        room: RoomId(9),
        tick,
        dt: Duration::from_secs_f64(1.0 / 30.0),
    }
}

fn tinfo(tick: u64) -> TickInfo {
    TickInfo {
        tick,
        at: Instant::now(),
    }
}

/// Drive one Join through `handle_msg`; returns the minted entity.
async fn join_direct(
    a: &mut ShardActor<TWorld, (), TState, TStrip>,
    conn: ConnectionId,
    epoch: u64,
    tick: u64,
) -> EntityId {
    let (reply_tx, reply_rx) = oneshot::channel();
    let (out_tx, _out_rx) = mpsc::channel::<FrameBatch>(8);
    assert!(
        a.handle_msg(
            ShardMsg::Join {
                conn,
                epoch,
                out: out_tx,
                reply: reply_tx
            },
            &tctx(tick)
        ),
        "a join must never stop the actor"
    );
    reply_rx
        .await
        .expect("join reply delivered")
        .expect("join ok")
        .0
}

fn ghost_migrate(
    conn: ConnectionId,
    epoch: u64,
    entity: EntityId,
    at_tick: u64,
) -> ShardMsg<TState, TStrip> {
    let (out, _out_rx) = mpsc::channel::<FrameBatch>(8);
    let (_act_tx, act_rx) = mpsc::channel::<Action>(8);
    ShardMsg::Migrate {
        from: 0,
        at_tick,
        wire: entity,
        state: TState {
            x: -7.0,
            y: 0.0,
            mode: 0,
        },
        player: Some(PlayerMigration {
            // Test identity policy: the conn id doubles as the player.
            player: PlayerId(conn.0),
            conn,
            epoch,
            entity,
            out,
            actions: act_rx,
            detached: false,
            detach_deadline: None,
            expire_to: crate::room::ExpireTo::Despawn,
            bot_fed: false,
            session_epoch: 0,
        }),
    }
}

/// Table-prune lock 1 — a Leave removes the connection's `conn_epoch`
/// entry in BOTH arms: the entity-matched despawn AND the broadcast
/// leave this shard held no matching entity for. The re-join path is
/// asserted too (the prune is only safe because re-joins and
/// migrate-ins re-insert).
#[tokio::test]
async fn leave_prunes_the_epoch_entry() {
    let mut a = bare_shard(0);

    // Arm 1: the entity-matched despawn.
    let conn = ConnectionId(4);
    let entity = join_direct(&mut a, conn, 7, 10).await;
    assert_eq!(a.conn_epoch.get(&conn), Some(&7));
    assert!(a.handle_msg(
        ShardMsg::Leave {
            conn,
            entity,
            epoch: 7
        },
        &tctx(11)
    ));
    assert!(
        !a.conn_epoch.contains_key(&conn),
        "the matched leave must prune the epoch entry"
    );
    assert_eq!(
        a.conn_tombstone.get(&conn),
        Some(&(7, 11)),
        "and write its tombstone (epoch, write tick)"
    );

    // Arm 2: the registry broadcasts every leave to all shards; here
    // the leave carries an entity this shard does NOT hold for that
    // connection (the stale-leave guard keeps the connection row —
    // it belongs to a live join), yet its epoch entry is still
    // pruned: the leave proves that join is dead HERE.
    let other = ConnectionId(5);
    let other_entity = join_direct(&mut a, other, 3, 12).await;
    assert_eq!(a.conn_epoch.get(&other), Some(&3));
    assert!(a.handle_msg(
        ShardMsg::Leave {
            conn: other,
            entity: other_entity.wrapping_add(1),
            epoch: 3
        },
        &tctx(13)
    ));
    assert!(
        a.conns.contains_key(&PlayerId(other.0)),
        "the stale-leave guard keeps the live join's row"
    );
    assert!(
        !a.conn_epoch.contains_key(&other),
        "the unmatched arm still prunes the epoch entry"
    );

    // Re-join safety (the prune's documented counterpart): a fresh
    // join carries a strictly newer epoch and re-inserts.
    join_direct(&mut a, other, 4, 14).await;
    assert_eq!(
        a.conn_epoch.get(&other),
        Some(&4),
        "re-join re-inserts the epoch entry"
    );
}

/// Table-prune lock 2 — the tombstone gate keeps rejecting a stale
/// Migrate within the TTL window, then the tombstone expires at the
/// first sweep past TTL + cadence, after which the same migration is
/// accepted again (the observable accept-path of expiry; the
/// migrate-in re-insertion of `conn_epoch` is asserted as well).
#[tokio::test]
async fn stale_migrate_rejected_then_tombstone_expires() {
    let mut a = bare_shard(1);
    let conn = ConnectionId(6);
    let wire = join_direct(&mut a, conn, 2, 100).await;
    assert!(a.handle_msg(
        ShardMsg::Leave {
            conn,
            entity: wire,
            epoch: 2
        },
        &tctx(101)
    ));
    assert_eq!(
        a.conn_tombstone.get(&conn),
        Some(&(2, 101)),
        "the leave wrote the tombstone (epoch, write tick)"
    );

    // The ghost arrives WITHIN the TTL window. `at_tick < ctx.tick`
    // so the install gate is open — only the epoch gate can stop it.
    // Existing behavior preserved: rejected.
    assert!(a.handle_msg(ghost_migrate(conn, 2, wire, 99), &tctx(102)));
    assert!(
        !a.conns.contains_key(&PlayerId(conn.0)),
        "the ghost migrate must not install the dead join"
    );
    assert_eq!(
        a.conn_tombstone.get(&conn),
        Some(&(2, 101)),
        "rejection leaves the tombstone untouched"
    );

    // Advance the clock through CONTROL phases (which run the lazy
    // sweep). First sweep ever → runs immediately at tick 200:
    // tombstone age 99 < TTL, kept. At tick 712 (512 past the last
    // sweep) the next sweep fires: age 611 >= TTL → expired.
    assert!(a.step_phases(&tinfo(200)));
    assert_eq!(
        a.conn_tombstone.get(&conn),
        Some(&(2, 101)),
        "inside the TTL window the sweep keeps the guard"
    );
    assert!(a.step_phases(&tinfo(712)));
    assert!(
        !a.conn_tombstone.contains_key(&conn),
        "past TTL + a sweep boundary the tombstone expires"
    );

    // Observable accept-path: the same (stale) migration now passes
    // the gate — proving expiry opened it — and migrate-in
    // re-inserts the pruned epoch entry (see CHANGE 1's comment).
    assert!(a.handle_msg(ghost_migrate(conn, 2, wire, 700), &tctx(713)));
    assert!(
        a.conns.contains_key(&PlayerId(conn.0)),
        "with the tombstone expired the gate no longer rejects"
    );
    assert_eq!(
        a.conn_epoch.get(&conn),
        Some(&2),
        "migrate-in re-inserts the epoch entry"
    );
}

/// Whether shard `s` reported `wire` as migrating out at tick `t-1`
/// (the subtlety: that despawn happens in tick `t`'s phase 4 — AFTER
/// the content observation — so the raw content of tick `t` still
/// shows it there, and the protocol ownership is "raw content minus
/// last tick's reports").
fn reported_out(migrated: &HashMap<(u64, usize), Vec<u64>>, t: u64, s: usize, wire: u64) -> bool {
    migrated
        .get(&(t.saturating_sub(1), s))
        .map(|v| v.contains(&wire))
        .unwrap_or(false)
}

/// The shard(s) that own `wire` at tick index `t` per the protocol.
fn owners_at(
    content: &[Vec<(u64, f32, f32, i8)>; 2],
    migrated: &HashMap<(u64, usize), Vec<u64>>,
    t: u64,
    wire: u64,
) -> Vec<usize> {
    (0..2)
        .filter(|&s| {
            content[s].iter().any(|(w, _, _, _)| *w == wire)
                && !reported_out(migrated, t, s, wire)
        })
        .collect()
}

/// Required test 1 — migration never drops or duplicates an entity,
/// including a ping-pong across the boundary. The entity walks right
/// (shard 0 → 1), then left (1 → 0), then right again: at EVERY tick
/// index it is in exactly one shard, and its position advances by
/// exactly the tick's mode (no teleports, no gaps — a one-tick loss
/// would show up as a double step).
#[tokio::test]
async fn migration_never_drops_or_duplicates() {
    let mut h = Harness::new();
    let conn = ConnectionId(1);
    let (wire, actions, _out) = h.join(0, conn, 1).await;
    // join() consumed tick 1: the entity is in shard 0 at x = -9.
    assert_eq!(h.content[0].len(), 1);
    assert_eq!(h.content[0][0].0, wire);
    let mut prev_x = h.content[0][0].1;

    // Walk right from x = -9; the crossing into shard 1 happens when
    // the post-step position reaches x = 0.
    h.act(&actions, conn, 1000).await;
    // Flip direction once in shard 1 (it crosses back), then again.
    let mut flipped = false;
    for _ in 0..40 {
        let c = h.tick().await;
        let t = h.t;
        // Exactly one shard owns the entity at this tick index.
        let owners = owners_at(&c, &h.migrated, t, wire);
        assert_eq!(
            owners.len(),
            1,
            "tick {t}: wire {wire} owned by {owners:?} (must be exactly one)"
        );
        let Some((x, mode)) = c[owners[0]].iter().find_map(|e| {
            if e.0 == wire {
                Some((e.1, e.3))
            } else {
                None
            }
        }) else {
            panic!("tick {t}: owner {} lost the entity", owners[0]);
        };
        // Position continuity: the step applied this tick equals the
        // mode in force for the tick (ingest runs before the step in
        // the same tick, so the tick's own content mode is the one
        // applied).
        assert!(
            (x - prev_x - mode as f32).abs() < 1e-6,
            "tick {t}: position jumped {prev_x} -> {x} (mode {mode})"
        );
        prev_x = x;
        // Flip direction once the entity is in shard 1 (it will cross
        // back), then again once it is back in shard 0.
        if owners[0] == 1 && !flipped {
            h.act(&actions, conn, 1001).await;
            flipped = true;
        } else if owners[0] == 0 && flipped {
            h.act(&actions, conn, 1000).await;
        }
    }
    // The ping-pong actually happened (the entity crossed into shard 1
    // and back).
    assert!(flipped, "the entity never crossed into shard 1");
    assert!(
        h.migrated
            .iter()
            .any(|((_, s), v)| *s == 1 && v.contains(&wire)),
        "the entity never crossed back into shard 0"
    );
}

/// Required test 2 — wire identity: a migrated entity keeps its id;
/// the two shards' id spaces are disjoint (no cross-shard collision).
#[tokio::test]
async fn wire_identity_stable_and_disjoint() {
    let mut h = Harness::new();
    let (w0, actions0, _o0) = h.join(0, ConnectionId(1), 1).await; // x = -9, shard 0
    let (w1, _a1, _o1) = h.join(1, ConnectionId(10), 1).await; // x = 0, shard 1
    // Disjoint ranges: shard 0 below 2^20, shard 1 at/above it.
    assert!(w0 < SHARD_SERIAL_RANGE, "shard 0 minted out of range: {w0}");
    assert!(
        (SHARD_SERIAL_RANGE..2 * SHARD_SERIAL_RANGE).contains(&w1),
        "shard 1 minted out of range: {w1}"
    );
    assert_ne!(w0, w1);
    // Walk w0 into shard 1; it must arrive under the SAME id.
    h.act(&actions0, ConnectionId(1), 1000).await;
    let mut crossed_at = None;
    for _ in 0..30 {
        let c = h.tick().await;
        if c[1].iter().any(|(w, _, _, _)| *w == w0)
            && !reported_out(&h.migrated, h.t, 1, w0)
        {
            crossed_at = Some(h.t);
            break;
        }
    }
    let Some(t) = crossed_at else {
        panic!("w0 never crossed into shard 1");
    };
    // The crossing was reported by shard 0 at t-1 (it despawns at t).
    assert!(
        reported_out(&h.migrated, t, 0, w0),
        "shard 0 did not report the crossing of w0"
    );
    // Both entities coexist in shard 1 under their own ids (no
    // collision: w0 and w1 are distinct records in the same world).
    let c = h.content;
    let ids: Vec<u64> = c[1].iter().map(|(w, _, _, _)| *w).collect();
    assert!(ids.contains(&w0) && ids.contains(&w1), "ids: {ids:?}");
    assert_eq!(ids.len(), 2);
}

/// Required test 3 — in-flight input survives the migration: the
/// action is in the channel while the connection is in transit; it is
/// applied by the RECEIVING shard (the channel moved with the
/// connection).
#[tokio::test]
async fn in_flight_action_survives_migration() {
    let mut h = Harness::new();
    let conn = ConnectionId(1);
    let (wire, actions, _out) = h.join(0, conn, 1).await;
    // Walk right; cross into shard 1.
    h.act(&actions, conn, 1000).await;
    let mut crossed_at = None;
    for _ in 0..30 {
        h.tick().await;
        if reported_out(&h.migrated, h.t, 0, wire) {
            crossed_at = Some(h.t);
            break;
        }
    }
    let Some(t_cross) = crossed_at else {
        panic!("no crossing");
    };
    // IN FLIGHT now: shard 0 just sent the migration (tick t_cross);
    // the connection halves are in transit (or just installed in
    // shard 1). Queue a mode change — it lands in the same channel
    // object the Migrate message carried to shard 1, wherever that
    // object currently sits.
    h.act(&actions, conn, 1001).await;
    // The next tick: shard 1 pulls it in its READ phase and applies it
    // (mode -1) in its step. If the action had been lost, the mode
    // would still be +1.
    let c = h.tick().await;
    let t = h.t;
    let owners = owners_at(&c, &h.migrated, t, wire);
    assert_eq!(owners.len(), 1, "tick {t}: owners {owners:?}");
    let (x, mode) = c[owners[0]]
        .iter()
        .find(|(w, _, _, _)| *w == wire)
        .map(|e| (e.1, e.3))
        .expect("entity present");
    assert_eq!(
        mode, -1,
        "the in-flight action was not applied by the receiving shard \
         (tick {t}, crossing at {t_cross}): mode {mode}"
    );
    // The entity stepped LEFT (toward shard 0) on this tick — the
    // in-flight action's mode, not the pre-migration one (+1). The
    // action was queued after the receiving shard's READ for the
    // spawn tick, so it is applied exactly one tick later: from x = 1
    // (the spawn tick's own +1 step) to x = 0.
    assert!((x - 0.0).abs() < 1e-6, "position {x} (expected 0)");
    // The action was ingested exactly once (shard 0's READ for the
    // crossing tick already ran before the send; only shard 1 can
    // pull it now).
    let ops = h.ops_drained().await;
    let n = ops.iter().filter(|(p, op)| *p == PlayerId(conn.0) && *op == 1001).count();
    assert_eq!(n, 1, "ops: {ops:?}");
}

/// Faz 2 lock — player identity is stable ACROSS SHARD MIGRATION: the
/// same human keeps ONE [`PlayerId`] from before the crossing to
/// after it, and the receiving shard ingests its input under that id
/// (the identity rides the `PlayerMigration`, exactly like the wire
/// id rides the entity state). Combined with the room-side resume
/// locks this pins the contract "resume/migration move the SESSION,
/// never the player".
#[tokio::test]
async fn player_identity_is_stable_across_migration() {
    let mut h = Harness::new();
    let conn = ConnectionId(1);
    let pid = PlayerId(conn.0); // the test logic's minting policy
    let (wire, actions, _out) = h.join(0, conn, 1).await;
    // One action BEFORE the migration: ingested by shard 0 under pid.
    h.act(&actions, conn, 1001).await;
    h.tick().await;
    // Walk right; cross into shard 1.
    h.act(&actions, conn, 1000).await;
    let mut crossed_at = None;
    for _ in 0..30 {
        h.tick().await;
        if reported_out(&h.migrated, h.t, 0, wire) {
            crossed_at = Some(h.t);
            break;
        }
    }
    assert!(crossed_at.is_some(), "no crossing");
    // Let the receiving shard install the row and pull one more input.
    h.tick().await;
    h.act(&actions, conn, 1002).await;
    h.tick().await;

    // Every observed op — on EITHER side of the seam — belongs to the
    // SAME stable player.
    let ops = h.ops_drained().await;
    assert!(
        ops.contains(&(pid, 1001)) && ops.contains(&(pid, 1002)),
        "input observed before AND after the migration: {ops:?}"
    );
    assert!(
        ops.iter().all(|(p, _)| *p == pid),
        "every action carries the SAME player id across migration: {ops:?}"
    );
}

/// Required test 4 — the leave/migration race: a `Migrate` whose join
/// is already dead (the leave was processed first) is rejected by the
/// epoch gate; a fresh join of the same connection (newer epoch) is
/// still accepted.
#[tokio::test]
async fn ghost_migrate_after_leave_is_rejected() {
    let mut h = Harness::new();
    let conn = ConnectionId(3);
    let (wire, _actions, _out) = h.join(0, conn, 1).await; // x = -7, shard 0, epoch 1
    let _ = h.tick().await; // steady
    // The leave (the registry's broadcast; epoch 1 = this join).
    h.leave(conn, wire, 1).await;
    let c = h.tick().await;
    // The entity is gone from both shards.
    assert!(
        c.iter().all(|v| v.iter().all(|(w, _, _, _)| *w != wire)),
        "the leave did not despawn the entity: {c:?}"
    );
    // The GHOST: a Migrate of the dead join (epoch 1) reaches shard 1
    // (simulating an in-flight migration that lost the race).
    let (ghost_out, _ghost_out_rx) = mpsc::channel::<FrameBatch>(8);
    let (_ghost_act_tx, ghost_act_rx) = mpsc::channel::<Action>(8);
    h.shard_txs[1]
        .send(ShardMsg::Migrate {
            from: 0,
            at_tick: h.t,
            wire,
            state: TState {
                x: -7.0,
                y: 0.0,
                mode: 0,
            },
            player: Some(PlayerMigration {
                player: PlayerId(conn.0),
                conn,
                epoch: 1,
                entity: wire,
                out: ghost_out,
                actions: ghost_act_rx,
                detached: false,
                detach_deadline: None,
                expire_to: crate::room::ExpireTo::Despawn,
                bot_fed: false,
                session_epoch: 0,
            }),
        })
        .await
        .expect("shard channel open");
    let c = h.tick().await;
    assert!(
        c.iter().all(|v| v.iter().all(|(w, _, _, _)| *w != wire)),
        "the ghost migrate resurrected the entity (the epoch gate \
         failed): {c:?}"
    );
    // A FRESH join of the same connection (epoch 2) must still be
    // accepted (the gate rejects only the dead join's epoch).
    let (wire2, _a2, _o2) = h.join(1, conn, 2).await;
    let c = h.content;
    assert!(
        c[1].iter().any(|(w, _, _, _)| *w == wire2),
        "the fresh join (epoch 2) was not accepted: {c:?}"
    );
    assert_ne!(wire, wire2, "fresh joins mint fresh identities");
}

/// Required test 5 — boundary visibility: each shard's snapshot
/// includes the neighbor's boundary entities (the borrowed set), so
/// a player at the boundary sees across the line.
#[tokio::test]
async fn boundary_entities_are_visible_to_both_sides() {
    let mut h = Harness::new();
    // conn 1 at x = -9 (shard 0), conn 11 at x = +1 (shard 1 — already
    // on the border: |x| <= 1).
    let (w0, actions0, mut out0) = h.join(0, ConnectionId(1), 1).await;
    let (w1, _a1, mut out1) = h.join(1, ConnectionId(11), 1).await;
    // Walk conn 1 toward the boundary (it reaches x = -1, exported by
    // shard 0, in a few ticks).
    h.act(&actions0, ConnectionId(1), 1000).await;

    /// Parse one snapshot frame into (wire, x, y) records.
    fn parse_records(payload: &bytes::Bytes) -> Vec<(u64, i32, i32)> {
        assert_eq!(payload.len() % 16, 0, "record-aligned payload");
        (0..payload.len() / 16)
            .map(|i| {
                let b = &payload[i * 16..i * 16 + 16];
                let wire = u64::from_le_bytes(b[0..8].try_into().unwrap());
                let x = i32::from_le_bytes(b[8..12].try_into().unwrap());
                let y = i32::from_le_bytes(b[12..16].try_into().unwrap());
                (wire, x, y)
            })
            .collect()
    }
    async fn read_snapshot(
        rx: &mut mpsc::Receiver<FrameBatch>,
    ) -> Option<Vec<(u64, i32, i32)>> {
        let batch = rx.recv().await.expect("snapshot stream alive");
        batch
            .into_iter()
            .find(|f| f.op == 0x7100)
            .map(|f| parse_records(&f.payload))
    }
    // Until each shard's snapshot shows BOTH w0 (shard 0's entity) and
    // w1 (shard 1's entity) — the own record under its own id and the
    // borrowed record under the neighbor's id (disjoint ranges: no
    // collision in the union view).
    let mut seen0: Option<Vec<(u64, i32, i32)>> = None;
    let mut seen1: Option<Vec<(u64, i32, i32)>> = None;
    for _ in 0..60 {
        if seen0.is_none()
            && let Some(r) = read_snapshot(&mut out0).await
            && r.iter().any(|(w, _, _)| *w == w0)
            && r.iter().any(|(w, _, _)| *w == w1)
        {
            seen0 = Some(r);
        }
        if seen1.is_none()
            && let Some(r) = read_snapshot(&mut out1).await
            && r.iter().any(|(w, _, _)| *w == w1)
            && r.iter().any(|(w, _, _)| *w == w0)
        {
            seen1 = Some(r);
        }
        if seen0.is_some() && seen1.is_some() {
            break;
        }
        let _ = h.tick().await;
    }
    let Some(r0) = seen0 else {
        panic!("shard 0's snapshot never included the borrowed entity w1");
    };
    let Some(r1) = seen1 else {
        panic!("shard 1's snapshot never included the borrowed entity w0");
    };
    // Each view has exactly the two records, under distinct wires.
    assert_eq!(r0.len(), 2, "shard 0 view: {r0:?}");
    assert_eq!(r1.len(), 2, "shard 1 view: {r1:?}");
    let wires0: Vec<u64> = r0.iter().map(|r| r.0).collect();
    let wires1: Vec<u64> = r1.iter().map(|r| r.0).collect();
    assert!(wires0.contains(&w0) && wires0.contains(&w1));
    assert!(wires1.contains(&w0) && wires1.contains(&w1));
}

// -----------------------------------------------------------------
// Faz 1 keep-alive promotion (behavior lock): a SILENT group on a
// SHARDED room receives its cached full snapshot on the keep-alive
// cadence — the shard-side mirror of the room actor's
// `unchanged_group_is_silent_until_keepalive`. Drives a bare
// (unspawned) [`ShardActor`] synchronously, like the table-prune
// locks above: the assertions read only the connection's wire bytes
// and the actor's own counters.
// -----------------------------------------------------------------

/// A single-group logic that goes silent once its content is out:
/// `on_join` dirties the group, an emission cleans it — so after the
/// join tick EVERY step is unchanged, which is exactly the state the
/// keep-alive cadence exists to interrupt.
struct KaLogic {
    dirty: bool,
    next_wire: u64,
}

impl GameLogic<TWorld> for KaLogic {
    type GroupKey = ();
    type Strip = TStrip;

    fn snapshot_op(&self) -> u16 {
        0x7180
    }
    fn private_op(&self) -> u16 {
        0x7181
    }
    fn group_of(&self, _w: &TWorld, _p: PlayerId) -> Self::GroupKey {}

    fn snapshot(
        &mut self,
        w: &mut TWorld,
        _ctx: &TickCtx,
        _g: &Self::GroupKey,
        _borrowed: &[BorderRecord<TStrip>],
        out: &mut bytes::BytesMut,
    ) -> bool {
        if !self.dirty {
            return false; // unchanged since the last emission
        }
        self.dirty = false;
        let mut recs: Vec<(u64, i32, i32)> = w
            .ents
            .iter()
            .map(|(wire, (x, y, _))| (*wire, *x as i32, *y as i32))
            .collect();
        recs.sort_unstable_by_key(|r| r.0);
        for (wire, x, y) in &recs {
            out.extend_from_slice(&wire.to_le_bytes());
            out.extend_from_slice(&x.to_le_bytes());
            out.extend_from_slice(&y.to_le_bytes());
        }
        true
    }

    fn on_join(&mut self, w: &mut TWorld, conn: ConnectionId) -> Admission {
        self.next_wire += 1;
        let x = (conn.0 % 20) as f32 - 10.0;
        w.ents.insert(self.next_wire, (x, 0.0, 0));
        self.dirty = true; // membership changed ⇒ must emit
        Admission {
            player: PlayerId(conn.0),
            entity: self.next_wire,
        }
    }

    fn on_leave(&mut self, _w: &mut TWorld, _player: PlayerId) {}
    fn ingest(&mut self, _w: &mut TWorld, _c: &TickCtx, actions: &mut Vec<Action>) {
        actions.clear();
    }
    fn update(&mut self, _w: &mut TWorld, _c: &TickCtx) {}
}

impl ShardLogic<TWorld> for KaLogic {
    type State = TState;

    fn index(&self) -> usize {
        0
    }
    fn shard_count(&self) -> usize {
        1
    }
    fn serial_base(&self) -> u64 {
        0
    }
    fn serial_range(&self) -> u64 {
        SHARD_SERIAL_RANGE
    }
    fn serial_used(&self) -> u64 {
        self.next_wire
    }
    fn neighbors(&self) -> &[usize] {
        &[]
    }
    fn collect_migrations(
        &mut self,
        _w: &mut TWorld,
        _nb: usize,
    ) -> Vec<Migrating<TState>> {
        Vec::new()
    }
    fn on_migrate_in(
        &mut self,
        _w: &mut TWorld,
        _wire: u64,
        _state: TState,
        _player: Option<PlayerId>,
    ) {
    }
    fn on_migrate_out(&mut self, _w: &mut TWorld, _wire: u64) {}
    fn collect_border(&self, _w: &TWorld) -> Vec<BorderRecord<TStrip>> {
        Vec::new()
    }
    fn own_wires(&self, w: &TWorld) -> Vec<u64> {
        w.ents.keys().copied().collect()
    }
}

/// The lock: `keepalive_hz = 10` under a 30 Hz shard → a re-send
/// every 3rd step. The group emits on the join step; steps 2..=8 are
/// silent EXCEPT steps 3 and 6, which ship the CACHED full snapshot
/// (byte-identical to the join emission) — and nothing else ever
/// reaches the wire. Mirrors the room-side semantics: same cadence
/// derivation, same cache, same resend counter.
#[tokio::test]
async fn sharded_keepalive_resends_cached_snapshot_to_silent_group() {
    let (_tick_tx, tick_rx) = broadcast::channel(64);
    let (_self_tx, rx) = channel::<ShardMsg<TState, TStrip>>(16);
    let (n0, _n0_rx) = channel::<ShardMsg<TState, TStrip>>(8);
    let (n1, _n1_rx) = channel::<ShardMsg<TState, TStrip>>(8);
    let mut a = ShardActor::new(
        RoomConfig {
            id: RoomId(11),
            tick_hz: 30.0,
            keepalive_hz: 10.0,
            metrics_cadence_hz: 0.0,
            ..Default::default()
        },
        0,
        TWorld::default(),
        Box::new(KaLogic {
            dirty: false,
            next_wire: 0,
        }),
        tick_rx,
        rx,
        vec![n0, n1],
        1,
        metrics_null(),
        None, // no result sink
    );

    // The join (its own CONTROL message; the group is dirty).
    let (out_tx, mut out_rx) = mpsc::channel::<FrameBatch>(16);
    let (reply_tx, reply_rx) = oneshot::channel();
    assert!(a.handle_msg(
        ShardMsg::Join {
            conn: ConnectionId(1),
            epoch: 1,
            out: out_tx,
            reply: reply_tx,
        },
        &tctx(1),
    ));
    let _wire = reply_rx.await.expect("join reply").expect("join ok");

    // Steps 2..=8 change nothing. Only the cadence steps (3 and 6)
    // may ship anything besides the join emission of step 1. Driven
    // through `step` (not `step_phases`) so the actor's own step
    // counter advances — the cadence is measured in ACTOR steps.
    assert!(a.step(&tinfo(1)), "step 1 runs");
    for t in 2..=8u64 {
        assert!(a.step(&tinfo(t)), "step {t} runs");
    }

    // Exactly three batches reached the member: the join emission plus
    // two keep-alive re-sends, all byte-identical (the CACHE, not a
    // fresh encode — the default hook re-sends `last`).
    let mut got = Vec::new();
    while let Ok(batch) = out_rx.try_recv() {
        got.push(batch);
    }
    assert_eq!(
        got.len(),
        3,
        "one emission (step 1) + two keep-alive re-sends (steps 3 and \
         6), nothing else: {got:?}"
    );
    let payloads: Vec<Vec<u8>> = got
        .iter()
        .map(|b| {
            assert_eq!(b.len(), 1, "one frame per batch");
            b[0].payload.to_vec()
        })
        .collect();
    for (i, p) in payloads.iter().enumerate() {
        assert_eq!(
            p, &payloads[0],
            "keep-alive re-send {i} must be the cached snapshot bytes"
        );
    }
    assert_eq!(
        payloads[0].len(),
        16,
        "the payload is one entity record (u64 wire LE + i32 x LE + \
         i32 y LE)"
    );
    // The mechanism counter agrees with the wire.
    assert_eq!(
        a.m.keepalive_resends, 2,
        "two unchanged-group re-sends (steps 3 and 6)"
    );
}

// -----------------------------------------------------------------
// Delta border exchange (CROSS-SHARD §6.4, the four-pin contract).
// The rig below wires two BARE (unspawned) actors with real bounded
// channels whose RECEIVING ends the test holds: every inter-shard
// message crosses the test's hands, so a loss can be simulated
// exactly (drop one message) and the resync traffic observed without
// any network or timing dependence.
// -----------------------------------------------------------------

// -----------------------------------------------------------------
// ShardLink seam structural locks (`docs/DISTRIBUTED.md` §3): the
// in-process link must be a TRANSPARENT wrapper — FIFO order
// preserved on drain, and a refused send hands the EXACT message back
// with no partial state (the migration rollback reads its payload out
// of the error alone).
// -----------------------------------------------------------------

/// Drive an [`InProcLink`] directly: FIFO order on drain; a send onto
/// a full link returns `LinkFull::Full` carrying the rejected message
/// without disturbing what is already queued; a drained link accepts
/// again; a link whose receive end is gone reports `Closed`.
#[test]
fn inproc_link_preserves_fifo_and_drop_semantics() {
    let (tx, rx) = channel::<ShardMsg<TState, TStrip>>(2);
    let mut link = InProcLink {
        tx: Some(tx),
        rx: Some(rx),
        mode: ExchangeMode::Delta,
    };
    // Fill to capacity, then one more: the third send must be refused
    // whole — nothing of it queued.
    for from in 0..2usize {
        assert!(
            link.send(ShardMsg::ResyncRequest { from }).is_ok(),
            "send {from} onto an empty/capacity-2 link"
        );
    }
    match link.send(ShardMsg::ResyncRequest { from: 2 }).unwrap_err() {
        LinkFull::Full {
            msg: ShardMsg::ResyncRequest { from },
        } => assert_eq!(from, 2, "the EXACT refused message comes back"),
        other => panic!("expected Full carrying the message, got {other:?}"),
    }
    // FIFO preserved and no partial state: exactly the two accepted
    // sends, in order; the rejected third did not squeeze in.
    let drained = link.drain();
    assert_eq!(drained.len(), 2, "only the accepted sends deliver");
    for (i, m) in drained.iter().enumerate() {
        match m {
            ShardMsg::ResyncRequest { from } => assert_eq!(*from, i),
            other => panic!("unexpected message in drain: {other:?}"),
        }
    }
    // An emptied link accepts again (the channel semantics, not a
    // poisoned wrapper).
    assert!(link.send(ShardMsg::ResyncRequest { from: 7 }).is_ok());
    assert!(link.send(ShardMsg::ResyncRequest { from: 8 }).is_ok());
    assert!(link.send(ShardMsg::ResyncRequest { from: 9 }).is_err());
    let drained = link.drain();
    assert_eq!(drained.len(), 2);
    assert!(matches!(drained[0], ShardMsg::ResyncRequest { from: 7 }));
    assert!(matches!(drained[1], ShardMsg::ResyncRequest { from: 8 }));
    assert!(link.drain().is_empty(), "drain empties fully");

    // Closed: a link whose receive end is gone refuses with `Closed`
    // (not Full), still handing the message back; a send-only link's
    // drain is simply empty.
    let (tx, rx) = channel::<ShardMsg<TState, TStrip>>(1);
    drop(rx);
    let mut dead = InProcLink {
        tx: Some(tx),
        rx: None,
        mode: ExchangeMode::AlwaysFull,
    };
    match dead.send(ShardMsg::ResyncRequest { from: 5 }).unwrap_err() {
        LinkFull::Closed {
            msg: ShardMsg::ResyncRequest { from },
        } => assert_eq!(from, 5),
        other => panic!("expected Closed carrying the message, got {other:?}"),
    }
    assert!(dead.drain().is_empty());
}

/// A TLogic for the border rig; its observation channels are dead
/// (every send is `let _ =` ignored) — the tests read protocol state,
/// not world observations.
fn rig_logic(index: usize) -> TLogic {
    let (obs, _obs_rx) = mpsc::channel(16);
    let (ops, _ops_rx) = mpsc::channel(16);
    TLogic {
        index,
        next_serial: 0,
        player_ent: HashMap::new(),
        ent_player: HashMap::new(),
        last_tick: 0,
        obs,
        ops,
    }
}

fn rig_actor(index: usize, neighbors: Vec<Mailbox<ShardMsg<TState, TStrip>>>) -> ShardActor<TWorld, (), TState, TStrip> {
    let (_tick_tx, tick_rx) = broadcast::channel(64);
    let (_self_tx, rx) = channel::<ShardMsg<TState, TStrip>>(16);
    ShardActor::new(
        RoomConfig {
            id: RoomId(13),
            keepalive_hz: 0.0,
            metrics_cadence_hz: 0.0,
            ..Default::default()
        },
        index,
        TWorld::default(),
        Box::new(rig_logic(index)),
        tick_rx,
        rx,
        neighbors,
        1,
        metrics_null(),
        None, // no result sink
    )
}

/// Two bare shard actors (0 and 1, mutual neighbors) wired through
/// channels the TEST controls. `put` seeds strip entities directly —
/// the strip is `|x| <= 1`, and x stays strictly inside the owner's
/// region so no migration path ever fires under these tests.
struct BorderRig {
    s0: ShardActor<TWorld, (), TState, TStrip>,
    s1: ShardActor<TWorld, (), TState, TStrip>,
    /// What s0 exports to s1 lands here (test-held receiving end).
    tx01: Mailbox<ShardMsg<TState, TStrip>>,
    rx01: Inbox<ShardMsg<TState, TStrip>>,
    /// What s1 sends back (resync requests) lands here.
    _tx10: Mailbox<ShardMsg<TState, TStrip>>,
    rx10: Inbox<ShardMsg<TState, TStrip>>,
}

impl BorderRig {
    /// DELTA-mode rig: the default for the §6.4 protocol locks, whose
    /// assertions are about upsert/exit/resync packaging.
    fn new() -> Self {
        Self::with_mode(ExchangeMode::Delta)
    }

    /// ALWAYS-FULL rig: Faz C's local-link derivation under test.
    fn new_always_full() -> Self {
        Self::with_mode(ExchangeMode::AlwaysFull)
    }

    fn with_mode(mode: ExchangeMode) -> Self {
        let (tx01, rx01) = mpsc::channel(16);
        let (tx10, rx10) = mpsc::channel(16);
        // Slot fillers for the unused self-slots (never targeted:
        // TLogic::neighbors is [1] for index 0 and [0] for index 1).
        let (d0, _d0rx) = channel::<ShardMsg<TState, TStrip>>(1);
        let (d1, _d1rx) = channel::<ShardMsg<TState, TStrip>>(1);
        let mut s0 = rig_actor(0, vec![d0, tx01.clone()]);
        let mut s1 = rig_actor(1, vec![tx10.clone(), d1]);
        // Both directions run the requested packaging (the self-slot
        // entries are never targeted; sizing the override to the
        // links vec keeps indexing trivially safe).
        s0.force_exchange_modes(vec![mode, mode]);
        s1.force_exchange_modes(vec![mode, mode]);
        BorderRig {
            s0,
            s1,
            tx01,
            rx01,
            _tx10: tx10,
            rx10,
        }
    }

    /// Run shard 0's phases at this tick index (its exports land in
    /// `rx01`).
    fn step0(&mut self, tick: u64) {
        assert!(self.s0.step_phases(&tinfo(tick)), "shard 0 keeps running");
    }

    /// Take everything shard 0 exported (WITHOUT delivering): the
    /// test inspects each message and decides deliver vs drop — the
    /// exact seam a lost exchange needs.
    fn drain01(&mut self) -> Vec<ShardMsg<TState, TStrip>> {
        let mut out = Vec::new();
        while let Ok(m) = self.rx01.try_recv() {
            out.push(m);
        }
        out
    }

    /// Feed messages into shard 1's CONTROL handler.
    fn deliver_to_s1(&mut self, msgs: Vec<ShardMsg<TState, TStrip>>) {
        for m in msgs {
            assert!(self.s1.handle_msg(m, &tctx(999)), "s1 keeps running");
        }
    }

    /// Feed messages into shard 0's CONTROL handler.
    fn deliver_to_s0(&mut self, msgs: Vec<ShardMsg<TState, TStrip>>) {
        for m in msgs {
            assert!(self.s0.handle_msg(m, &tctx(999)), "s0 keeps running");
        }
    }
}

/// Seed a boundary entity straight into a shard's world.
fn put(a: &mut ShardActor<TWorld, (), TState, TStrip>, wire: u64, x: f32, y: f32) {
    a.world.ents.insert(wire, (x, y, 0));
}

/// Assert the batch is exactly one Border carrying a Full; return its
/// (seq, entities). Generic over the strip payload so every rig
/// (positional and rich) reuses one helper.
fn expect_full<S: Debug>(msgs: &[ShardMsg<TState, S>]) -> (u64, &[BorderRecord<S>]) {
    assert_eq!(msgs.len(), 1, "exactly one export message: {msgs:?}");
    match &msgs[0] {
        ShardMsg::Border {
            exchange: BorderExchange::Full { seq, entities, .. },
            ..
        } => (*seq, entities.as_slice()),
        other => panic!("expected a Full exchange, got {other:?}"),
    }
}

/// Assert the batch is exactly one Border carrying a Delta; return
/// its (seq, upserts, exits). Generic over the strip payload.
fn expect_delta<S: Debug>(
    msgs: &[ShardMsg<TState, S>],
) -> (u64, &[BorderRecord<S>], &[u64]) {
    assert_eq!(msgs.len(), 1, "exactly one export message: {msgs:?}");
    match &msgs[0] {
        ShardMsg::Border {
            exchange:
                BorderExchange::Delta {
                    seq,
                    upserts,
                    exits,
                    ..
                },
            ..
        } => (*seq, upserts.as_slice(), exits.as_slice()),
        other => panic!("expected a Delta exchange, got {other:?}"),
    }
}

/// Delta lock 1 — an entity entering the strip appears in the
/// neighbor's view; moving updates it in place; leaving removes it
/// (no ghost). The bootstrap is an explicit Full; every later step is
/// a minimal delta.
#[tokio::test]
async fn delta_exchange_applies_upserts_and_exits_correctly() {
    let mut r = BorderRig::new();

    // Enter: first contact ships the whole strip as a Full...
    put(&mut r.s0, 100, -1.0, 0.0);
    r.step0(1);
    let msgs = r.drain01();
    let (seq, entities) = expect_full(&msgs);
    assert_eq!(
        entities,
        [rec(100, -1, 0)],
        "bootstrap Full carries the strip"
    );
    r.deliver_to_s1(msgs);
    assert_eq!(r.s1.border[&0].recs.len(), 1, "view established");
    assert_eq!(
        r.s1.border[&0].expected_seq,
        seq + 1,
        "the receiver expects the next sequence"
    );

    // Move: only the changed record ships, as an upsert delta.
    put(&mut r.s0, 100, -1.0, 1.0);
    r.step0(2);
    let msgs = r.drain01();
    let (_seq2, upserts, exits) = expect_delta(&msgs);
    assert_eq!(upserts, [rec(100, -1, 1)]);
    assert!(exits.is_empty(), "a move is not an exit");
    r.deliver_to_s1(msgs);
    assert_eq!(r.s1.border[&0].recs[&100].state.y, 1, "position updated");

    // A second entity enters: only IT is new.
    put(&mut r.s0, 101, -1.0, 5.0);
    r.step0(3);
    let msgs = r.drain01();
    let (_seq3, upserts, _exits3) = expect_delta(&msgs);
    assert_eq!(upserts, [rec(101, -1, 5)]);
    r.deliver_to_s1(msgs);
    assert_eq!(r.s1.border[&0].recs.len(), 2);

    // Leave: an explicit exit record — the borrowed view must drop
    // the entity (a full-era wholesale replace never had this failure
    // mode; a delta without exits would ghost forever).
    let _ = r.s0.world.ents.remove(&101);
    r.step0(4);
    let msgs = r.drain01();
    let (_seq4, upserts, exits) = expect_delta(&msgs);
    assert!(upserts.is_empty(), "a leave is not an upsert");
    assert_eq!(exits, [101]);
    r.deliver_to_s1(msgs);
    assert_eq!(
        r.s1.border[&0].recs.len(),
        1,
        "no ghost after the exit"
    );
    assert!(!r.s1.border[&0].recs.contains_key(&101));
}

/// Delta lock 2 (§6.4 pin 3a) — a lost delta is DETECTED, not silently
/// diverged: the receiver rejects the next delta on its sequence
/// mismatch, quarantines the view, sends a ResyncRequest upstream, and
/// the serving Full restores a correct complete view.
#[tokio::test]
async fn seq_gap_triggers_resync_full() {
    let mut r = BorderRig::new();

    // Bootstrap: Full(seq=1) delivered → expected becomes 2.
    put(&mut r.s0, 100, -1.0, 0.0);
    r.step0(1);
    let msgs = r.drain01();
    r.deliver_to_s1(msgs);

    // THE LOSS: the next delta (seq=2, y→1) never arrives.
    put(&mut r.s0, 100, -1.0, 1.0);
    r.step0(2);
    let lost = r.drain01();
    assert_eq!(lost.len(), 1, "the delta was sent — then dropped by us");
    // ...and discarded. Nothing delivered.

    // The NEXT delta (seq=3) carries the wrong sequence number.
    put(&mut r.s0, 100, -1.0, 2.0);
    r.step0(3);
    let msgs = r.drain01();
    let (seq, _, _) = expect_delta(&msgs);
    assert_eq!(seq, 3, "the sender stamped consecutively");
    r.deliver_to_s1(msgs); // rejected INSIDE handle_msg

    assert!(
        r.s1.border[&0].stale_until_full,
        "the mismatch quarantines the view"
    );
    assert_eq!(
        r.s1.border[&0].recs[&100].state.y, 0,
        "nothing after the last GOOD exchange was applied (no \
         half-applied state)"
    );
    assert_eq!(
        r.s1.bstats.resync_requests_sent, 1,
        "exactly one resync request went upstream"
    );
    // The request crossed back over the controlled channel:
    let requests: Vec<_> = {
        let mut out = Vec::new();
        while let Ok(m) = r.rx10.try_recv() {
            out.push(m);
        }
        out
    };
    assert!(
        requests
            .iter()
            .any(|m| matches!(m, ShardMsg::ResyncRequest { from: 1 })),
        "ResyncRequest flowed to the neighbor: {requests:?}"
    );
    r.deliver_to_s0(requests);

    // The healing Full: even with NO further changes the flagged
    // neighbor gets a Full next tick, and it restores the COMPLETE
    // current truth (including what the lost delta carried).
    r.step0(4);
    let msgs = r.drain01();
    let (_, entities) = expect_full(&msgs);
    assert_eq!(
        entities,
        [rec(100, -1, 2)],
        "the healing Full re-baselines everything"
    );
    r.deliver_to_s1(msgs);
    assert!(!r.s1.border[&0].stale_until_full, "quarantine lifted");
    assert_eq!(r.s1.border[&0].recs[&100].state.y, 2, "view correct again");
    assert!(
        !r.s1.border[&0].stale_until_full
            && r.s1.border[&0].expected_seq == 5,
        "sequence re-baselined past the healing Full"
    );
}

/// Delta lock 3 (pin 3b) — a rebuilt shard's fresh incarnation leads
/// with a FULL (its sender state starts empty), and the receiver
/// resets cleanly: the dead incarnation's records cannot ghost.
#[tokio::test]
async fn rebuilt_shard_first_exchange_is_full_and_resets_receiver() {
    let mut r = BorderRig::new();

    // Incarnation A establishes a populated view on shard 1.
    put(&mut r.s0, 100, -1.0, 0.0);
    r.step0(1);
    let msgs = r.drain01();
    r.deliver_to_s1(msgs);
    assert_eq!(r.s1.border[&0].recs.len(), 1);

    // REBUILD: a brand-new actor for shard 0 — fresh world (the new
    // incarnation respawned different entities), fresh export state,
    // its own channel to the SAME receiver.
    let (tx01p, mut rx01p) = mpsc::channel(16);
    let (d, _drx) = channel::<ShardMsg<TState, TStrip>>(1);
    std::mem::forget(_drx);
    let mut s0p = rig_actor(0, vec![d, tx01p]);
    put(&mut s0p, 200, -1.0, 7.0);
    assert!(s0p.step_phases(&tinfo(50)), "rebuilt shard runs");

    let mut first = Vec::new();
    while let Ok(m) = rx01p.try_recv() {
        first.push(m);
    }
    let (seq, entities) = expect_full(&first);
    assert_eq!(
        entities,
        [rec(200, -1, 7)],
        "the FRESH incarnation's first exchange is a Full of ITS strip"
    );
    r.deliver_to_s1(first);

    // The receiver reset cleanly: exactly the new incarnation's
    // records, old-incarnation ghost gone, sequence re-baselined.
    let view = &r.s1.border[&0];
    assert_eq!(view.recs.len(), 1, "whole-view replacement: {view:?}");
    assert!(view.recs.contains_key(&200), "new entity present");
    assert!(
        !view.recs.contains_key(&100),
        "the dead incarnation's record must not survive as a ghost"
    );
    assert_eq!(
        view.expected_seq,
        seq + 1,
        "expected sequence re-baselined from the new stream"
    );
    assert!(!view.stale_until_full);
}

/// Delta lock 4 (pin 3c) — the periodic sigorta: a quiet neighbor is
/// shipped NOTHING on ordinary ticks (the byte win), but the 256-tick
/// cadence forces a Full even with zero changes.
#[tokio::test]
async fn periodic_full_fires_on_cadence() {
    let mut r = BorderRig::new();

    // Bootstrap + one change establish a non-empty ledger.
    put(&mut r.s0, 100, -1.0, 0.0);
    r.step0(1);
    {
        let msgs = r.drain01();
        r.deliver_to_s1(msgs);
    }
    put(&mut r.s0, 100, -1.0, 1.0);
    r.step0(2);
    {
        let msgs = r.drain01();
        r.deliver_to_s1(msgs);
    }

    // Quiet tick: no changes ⇒ NOTHING ships (this skip is the point
    // of the whole exercise).
    r.step0(3);
    assert!(r.drain01().is_empty(), "an unchanged strip ships nothing");

    // ...but the cadence tick forces a Full regardless of quietness.
    assert!(r.s0.world.ents.len() == 1, "still just the one entity");
    r.step0(BORDER_FULL_EVERY_TICKS);
    let msgs = r.drain01();
    let (_, entities) = expect_full(&msgs);
    assert_eq!(entities.len(), 1, "the Full carries the whole strip");

    // And quietness resumes right after.
    r.step0(BORDER_FULL_EVERY_TICKS + 1);
    assert!(r.drain01().is_empty(), "no change after the cadence ⇒ silent");

    // Counter cross-check within this run: two Fulls (bootstrap +
    // periodic), one delta, zero drops.
    assert_eq!(r.s0.bstats.full_exchanges, 2);
    assert_eq!(r.s0.bstats.delta_exchanges, 1);
    assert_eq!(r.s0.bstats.export_drops, 0);
}

/// Delta lock 5 (backpressure correctness) — a try_send failure on a
/// DELTA marks that neighbor for a Full, which arrives on the very
/// next tick carrying the data the dropped delta would have brought:
/// divergence heals within ONE tick instead of the 256-tick cadence.
#[tokio::test]
async fn send_failure_marks_neighbor_for_full_resync() {
    let mut r = BorderRig::new();

    // Bootstrap normally.
    put(&mut r.s0, 100, -1.0, 0.0);
    r.step0(1);
    {
        let msgs = r.drain01();
        r.deliver_to_s1(msgs);
    }

    // Saturate the neighbor mailbox: nothing else fits.
    while r
        .tx01
        .try_send(ShardMsg::ResyncRequest { from: 999 })
        .is_ok()
    {}

    // A strip change now ships a delta — which MUST fail.
    put(&mut r.s0, 100, -1.0, 1.0);
    r.step0(2);
    assert_eq!(
        r.s0.bstats.delta_drops, 1,
        "the failed delta is counted"
    );
    assert!(
        r.s0.export[&1].needs_full,
        "the failure flags the neighbor for a Full"
    );

    // Unblock the channel (drain the dummies AND anything else).
    while r.rx01.try_recv().is_ok() {}

    // Next tick, NO further changes: the flag alone forces a Full —
    // and it carries the position update the dropped delta had.
    r.step0(3);
    let msgs = r.drain01();
    let (_, entities) = expect_full(&msgs);
    assert_eq!(
        entities,
        [rec(100, -1, 1)],
        "the healing Full contains what the dropped delta carried"
    );
    r.deliver_to_s1(msgs);
    assert_eq!(
        r.s1.border[&0].recs[&100].state.y, 1,
        "the receiver converged despite the loss"
    );
    assert!(!r.s0.export[&1].needs_full, "flag consumed");
}

/// Delta lock 6 (pin 4) — the own-wins filter applies IDENTICALLY to
/// records that entered the view via a delta: an entity that just
/// migrated INTO this shard wins over the neighbor's stale borrowed
/// copy, so the snapshot lists it once, at the OWN position.
#[tokio::test]
async fn own_wins_filter_applies_to_delta_applied_records() {
    let mut r = BorderRig::new();

    // Shard 1 gains its own member at x = 0 (conn 10 ⇒ x = 0 per the
    // test logic's spawn rule; region 1, so nothing migrates).
    let (out_tx, mut out_rx) = mpsc::channel::<FrameBatch>(16);
    let (reply_tx, reply_rx) = oneshot::channel();
    assert!(r.s1.handle_msg(
        ShardMsg::Join {
            conn: ConnectionId(10),
            epoch: 1,
            out: out_tx,
            reply: reply_tx,
        },
        &tctx(1),
    ));
    let w_own = reply_rx
        .await
        .expect("join reply")
        .expect("join ok")
        .0;

    // Bootstrap an EMPTY strip from shard 0 (Full, first contact),
    // then apply a DELTA that inserts the stale borrowed copy of the
    // just-crossed own entity — the exact crossing-tick shape of pin
    // 4. The copy enters the view THROUGH the delta path.
    r.step0(1); // empty strip, first contact ⇒ Full{entities: []}
    {
        let msgs = r.drain01();
        r.deliver_to_s1(msgs);
    }
    r.s1.handle_msg(
        ShardMsg::Border {
            from: 0,
            exchange: BorderExchange::Delta {
                seq: 2, // matches the expected sequence after the Full
                tick: 2,
                upserts: vec![rec(w_own, -9, 0)],
                exits: vec![],
            },
        },
        &tctx(2),
    );
    assert_eq!(
        r.s1.border[&0].recs.get(&w_own).map(|b| b.state.x),
        Some(-9),
        "the stale copy IS in the borrowed view (delta applied)"
    );

    // Broadcast: the snapshot must contain the entity EXACTLY ONCE,
    // at the OWN (fresh) position — the borrowed copy filtered.
    assert!(r.s1.step_phases(&tinfo(3)));
    let mut seen = Vec::new();
    while let Ok(batch) = out_rx.try_recv() {
        for f in batch {
            if f.op == 0x7100 {
                seen.extend_from_slice(&f.payload);
            }
        }
    }
    assert_eq!(
        seen.len(),
        16,
        "one 16-byte record total (own + filtered borrowed)"
    );
    let wire = u64::from_le_bytes(seen[0..8].try_into().unwrap());
    let x = i32::from_le_bytes(seen[8..12].try_into().unwrap());
    let y = i32::from_le_bytes(seen[12..16].try_into().unwrap());
    assert_eq!(
        (wire, x, y),
        (w_own, 0, 0),
        "the OWN record won over the delta-applied stale copy"
    );
}
// -----------------------------------------------------------------
// Rich-strip locks: the visibility-strip payload is the GAME's type
// ([`GameLogic::Strip`]). These locks prove the generalization does
// what the core-fixed record could not: a payload field beyond
// position must survive BOTH exchange paths (Full bootstrap and
// Delta upsert), and a change in ANY payload field — not just the
// coordinates — must fire the delta diff.
// -----------------------------------------------------------------

/// A strip payload with one field BEYOND position (a facing-like
/// quantity; the combat/prediction shape this generalization exists
/// for).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TRich {
    x: i32,
    y: i32,
    facing: i16,
}

/// A minimal shard logic whose strip carries [`TRich`]. The payload is
/// assembled from game state (`TWorld`'s third slot read as facing) —
/// exactly the ownership split under test: the core could never have
/// derived this field. `update` is a no-op, so entities stay put and
/// only an explicit mutation changes anything.
struct RichLogic {
    index: usize,
}

impl GameLogic<TWorld> for RichLogic {
    type GroupKey = ();
    type Strip = TRich;

    fn snapshot_op(&self) -> u16 {
        0x7300
    }
    fn private_op(&self) -> u16 {
        0x7301
    }
    fn group_of(&self, _w: &TWorld, _p: PlayerId) -> Self::GroupKey {}
    fn snapshot(
        &mut self,
        _w: &mut TWorld,
        _c: &TickCtx,
        _g: &Self::GroupKey,
        _borrowed: &[BorderRecord<TRich>],
        _out: &mut bytes::BytesMut,
    ) -> bool {
        false // no members join in these tests; nothing ever emits
    }
    fn on_join(&mut self, w: &mut TWorld, conn: ConnectionId) -> Admission {
        let wire = self.index as u64 * SHARD_SERIAL_RANGE + conn.0;
        w.ents.insert(wire, ((conn.0 % 20) as f32 - 10.0, 0.0, 0));
        Admission {
            player: PlayerId(conn.0),
            entity: wire,
        }
    }
    fn on_leave(&mut self, _w: &mut TWorld, _player: PlayerId) {}
    fn ingest(&mut self, _w: &mut TWorld, _c: &TickCtx, actions: &mut Vec<Action>) {
        actions.clear();
    }
    fn update(&mut self, _w: &mut TWorld, _c: &TickCtx) {}
}

impl ShardLogic<TWorld> for RichLogic {
    type State = TState;

    fn index(&self) -> usize {
        self.index
    }
    fn shard_count(&self) -> usize {
        2
    }
    fn serial_base(&self) -> u64 {
        self.index as u64 * SHARD_SERIAL_RANGE
    }
    fn serial_range(&self) -> u64 {
        SHARD_SERIAL_RANGE
    }
    fn serial_used(&self) -> u64 {
        0
    }
    fn neighbors(&self) -> &[usize] {
        if self.index == 0 {
            &[1]
        } else {
            &[0]
        }
    }
    fn collect_migrations(
        &mut self,
        _w: &mut TWorld,
        _nb: usize,
    ) -> Vec<Migrating<TState>> {
        Vec::new()
    }
    fn on_migrate_in(
        &mut self,
        _w: &mut TWorld,
        _wire: u64,
        _state: TState,
        _player: Option<PlayerId>,
    ) {
    }
    fn on_migrate_out(&mut self, _w: &mut TWorld, _wire: u64) {}
    fn collect_border(&self, w: &TWorld) -> Vec<BorderRecord<TRich>> {
        // The strip: same |x| <= 1 frame as TLogic, plus the CUSTOM
        // field from game state.
        w.ents
            .iter()
            .filter(|(_, (x, _, _))| x.abs() <= 1.0)
            .map(|(wire, (x, y, facing))| BorderRecord {
                wire: *wire,
                state: TRich {
                    x: *x as i32,
                    y: *y as i32,
                    facing: *facing as i16,
                },
            })
            .collect()
    }
    fn own_wires(&self, w: &TWorld) -> Vec<u64> {
        w.ents.keys().copied().collect()
    }
}

/// Two bare [`RichLogic`] actors wired through channels the TEST
/// controls (the [`BorderRig`] pattern, typed over [`TRich`]).
/// Faz C lock 1 — same-process links run ALWAYS-FULL packaging: a
/// changed strip ships a complete Full every tick (no deltas on the
/// wire, no dirty suppression), because bytes over an mpsc move are
/// free while the diffing CPU was measured at 40-55x the full cost.
#[tokio::test]
async fn local_link_exchanges_are_always_full() {
    let mut r = BorderRig::new_always_full();
    put(&mut r.s0, 100, -1.0, 0.0);
    r.step0(1);
    let msgs = r.drain01();
    let (_seq, entities) = expect_full(&msgs);
    assert_eq!(entities.len(), 1, "bootstrap ships the strip as Full");

    // A move next tick ships ANOTHER Full — wholesale replacement,
    // never a Delta upsert.
    put(&mut r.s0, 100, -1.0, 1.0);
    r.step0(2);
    let msgs = r.drain01();
    let (_seq, entities) = expect_full(&msgs);
    assert_eq!(entities.len(), 1);

    r.deliver_to_s1(msgs);
    assert_eq!(r.s1.border[&0].recs.len(), 1, "view established");
    assert_eq!(
        r.s1.border[&0].recs[&100].state.y, 1,
        "the relocated position arrived"
    );
}

/// Faz C lock 2 — ALWAYS-FULL mode keeps the borrowed view exact
/// across UNCHANGED ticks too: wholesale replacement cannot ghost,
/// duplicate, or drift, locking the evaporation-guard under this
/// mode against future regressions.
#[tokio::test]
async fn always_full_keeps_view_exact_across_unchanged_ticks() {
    let mut r = BorderRig::new_always_full();
    put(&mut r.s0, 100, -1.0, 0.0);
    r.step0(1);
    let msgs = r.drain01();
    r.deliver_to_s1(msgs);

    // Three ticks with NOTHING changed: each still ships a Full of
    // the identical single record, and the receiving view never
    // grows beyond it.
    for tick in 2..=4 {
        r.step0(tick);
        let msgs = r.drain01();
        let (_seq, entities) = expect_full(&msgs);
        assert_eq!(entities.len(), 1, "tick {tick} ships the strip");
        r.deliver_to_s1(msgs);
        assert_eq!(
            r.s1.border[&0].recs.len(), 1,
            "view stays exactly one record at tick {tick}"
        );
    }
}

/// Faz C lock 3 — OPPOSITE DIRECTIONS may run different packagings
/// (s0->s1 forced ALWAYS-FULL while s1->s0 runs DELTA): each receiver
/// applies its own inbound variant correctly and both views stay
/// exact. This per-link independence is what the mode derivation
/// relies on when future Ipc/Net links mix with local ones.
#[tokio::test]
async fn opposite_directions_run_different_packagings() {
    let mut r = BorderRig::with_mode(ExchangeMode::Delta);
    // Asymmetric forcing: s0's outbound slot runs ALWAYS-FULL while
    // s1's outbound slot runs DELTA.
    r.s0.force_exchange_modes(vec![ExchangeMode::AlwaysFull, ExchangeMode::AlwaysFull]);
    r.s1.force_exchange_modes(vec![ExchangeMode::Delta, ExchangeMode::Delta]);

    // Entities on BOTH sides, so both directions have content.
    put(&mut r.s0, 100, -1.0, 0.0);
    put(&mut r.s1, 200, 1.0, 0.0);

    let tinfo = |tick: u64| TickInfo { tick, at: Instant::now() };

    // Tick 5: both shards step and bootstrap (needs_full ⇒ Full lead).
    r.step0(5);
    let _ = r.s1.step_phases(&tinfo(5));
    // Deliver each direction's exchanges so views establish BEFORE
    // the assertions: delivery feeds inboxes, processing happens on
    // the NEXT step.
    for m in r.drain01() { r.deliver_to_s1(vec![m]); }
    for m in { let mut v=Vec::new(); while let Ok(m)=r.rx10.try_recv(){v.push(m);} v } {
        r.deliver_to_s0(vec![m]);
    }
    r.step0(6);
    let _ = r.s1.step_phases(&tinfo(6));
    assert_eq!(r.s1.border[&0].recs.len(), 1, "s1 sees s0's entity");
    assert_eq!(r.s0.border[&1].recs.len(), 1, "s0 sees s1's entity");

    // Move each entity; next ticks ship per-mode packaging.
    put(&mut r.s0, 100, -1.0, 2.0);
    put(&mut r.s1, 200, 1.0, 2.0);
    r.step0(7);
    let _ = r.s1.step_phases(&tinfo(7));
    let m01 = r.drain01();
    let mut m10: Vec<_> = Vec::new();
    while let Ok(m) = r.rx10.try_recv() { m10.push(m); }

    assert!(
        m01.iter().any(|m| matches!(m,
            ShardMsg::Border { exchange: BorderExchange::Full { .. }, .. })),
        "AlwaysFull direction keeps shipping fulls"
    );
    assert!(
        m10.iter().any(|m| matches!(m,
            ShardMsg::Border { exchange: BorderExchange::Delta { .. }, .. })),
        "Delta direction ships an upsert"
    );
    for m in m01 { r.deliver_to_s1(vec![m]); }
    for m in m10 { r.deliver_to_s0(vec![m]); }

    // Views converge to the moved positions.
    r.step0(8);
    let _ = r.s1.step_phases(&tinfo(8));
    assert_eq!(
        r.s0.border[&1].recs[&200].state.y, 2,
        "s0's borrowed view took s1's update"
    );
    assert_eq!(
        r.s1.border[&0].recs[&100].state.y, 2,
        "s1's borrowed view took s0's update"
    );
}

struct RichRig {
    s0: ShardActor<TWorld, (), TState, TRich>,
    s1: ShardActor<TWorld, (), TState, TRich>,
    /// What s0 exports to s1 lands here (test-held receiving end).
    rx01: Inbox<ShardMsg<TState, TRich>>,
    /// What s1 sends back lands here (never drained yet: no rich
    /// lock exercises the resync round trip; held so the channel
    /// stays open).
    #[allow(dead_code)]
    _rx10: Inbox<ShardMsg<TState, TRich>>,
}

impl RichRig {
    fn new() -> Self {
        let (tx01, rx01) = mpsc::channel(16);
        let (tx10, _rx10) = mpsc::channel(16);
        let build = |index: usize, tx: Mailbox<ShardMsg<TState, TRich>>, other: Mailbox<ShardMsg<TState, TRich>>| {
            let (_tick_tx, tick_rx) = broadcast::channel(64);
            let (_self_tx, rx) = channel::<ShardMsg<TState, TRich>>(16);
            ShardActor::new(
                RoomConfig {
                    id: RoomId(15),
                    keepalive_hz: 0.0,
                    metrics_cadence_hz: 0.0,
                    ..Default::default()
                },
                index,
                TWorld::default(),
                Box::new(RichLogic { index }),
                tick_rx,
                rx,
                vec![tx, other],
                1,
                metrics_null(),
                None, // no result sink
            )
        };
        let (d0, _d0rx) = mpsc::channel(1);
        let (d1, _d1rx) = mpsc::channel(1);
        let mut s0 = build(0, d0, tx01.clone());
        let mut s1 = build(1, tx10, d1);
        // The rich locks exercise DELTA packaging (upsert/exit
        // bookkeeping over the custom Strip fields) — force Delta on
        // the live directions (Faz C made local links default to
        // AlwaysFull).
        s0.force_exchange_modes(vec![ExchangeMode::AlwaysFull, ExchangeMode::Delta]);
        s1.force_exchange_modes(vec![ExchangeMode::Delta, ExchangeMode::AlwaysFull]);
        RichRig { s0, s1,
            rx01,
            _rx10,
        }
    }

    /// Run shard 0's phases at this tick index (its exports land in
    /// `rx01`).
    fn step0(&mut self, tick: u64) {
        assert!(self.s0.step_phases(&tinfo(tick)), "shard 0 keeps running");
    }

    /// Take everything shard 0 exported (WITHOUT delivering).
    fn drain01(&mut self) -> Vec<ShardMsg<TState, TRich>> {
        let mut out = Vec::new();
        while let Ok(m) = self.rx01.try_recv() {
            out.push(m);
        }
        out
    }

    /// Feed messages into shard 1's CONTROL handler.
    fn deliver_to_s1(&mut self, msgs: Vec<ShardMsg<TState, TRich>>) {
        for m in msgs {
            assert!(self.s1.handle_msg(m, &tctx(999)), "s1 keeps running");
        }
    }
}

/// Seed a boundary entity straight into a shard's world (the third
/// tuple slot is the FACING source for [`RichLogic`]'s strip).
fn put_rich(a: &mut ShardActor<TWorld, (), TState, TRich>, wire: u64, x: f32, facing: i8) {
    a.world.ents.insert(wire, (x, 0.0, facing));
}

/// Rich lock 1 — a strip record whose payload has a field beyond
/// position arrives INTACT through both paths: the Full bootstrap on
/// first contact, and the Delta upsert after ONLY the custom field
/// changed. The receiving view (typed over the SAME logic-defined
/// payload) holds the exact values the sender's logic assembled.
#[tokio::test]
async fn rich_strip_record_survives_full_and_delta_paths() {
    let mut r = RichRig::new();

    // Bootstrap: first contact ships the whole strip as a Full, with
    // the custom field intact.
    put_rich(&mut r.s0, 100, -1.0, 7);
    r.step0(1);
    let msgs = r.drain01();
    let (seq, entities) = expect_full(&msgs);
    assert_eq!(
        entities,
        [BorderRecord {
            wire: 100,
            state: TRich {
                x: -1,
                y: 0,
                facing: 7
            }
        }],
        "the Full bootstrap carries the RICH record whole"
    );
    r.deliver_to_s1(msgs);
    assert_eq!(
        r.s1.border[&0].recs[&100].state,
        TRich {
            x: -1,
            y: 0,
            facing: 7
        },
        "the FULL path preserved every payload field"
    );
    assert_eq!(
        r.s1.border[&0].expected_seq,
        seq + 1,
        "receiver sequence re-baselined by the Full"
    );

    // Change ONLY the custom field (position untouched): the next
    // exchange is a delta whose upsert carries the new value intact.
    r.s0.world.ents.get_mut(&100).unwrap().2 = 9;
    r.step0(2);
    let msgs = r.drain01();
    let (_seq2, upserts, exits) = expect_delta(&msgs);
    assert_eq!(
        upserts,
        [BorderRecord {
            wire: 100,
            state: TRich {
                x: -1,
                y: 0,
                facing: 9
            }
        }],
        "the DELTA upsert carries the custom field"
    );
    assert!(exits.is_empty());
    r.deliver_to_s1(msgs);
    assert_eq!(
        r.s1.border[&0].recs[&100].state.facing, 9,
        "the DELTA path preserved the custom field end to end"
    );
    assert_eq!(
        (r.s1.border[&0].recs[&100].state.x, r.s1.border[&0].recs[&100].state.y),
        (-1, 0),
        "position unchanged alongside it"
    );
}

/// Rich lock 2 — the delta diff keys off WHOLE-payload equality: a
/// change confined to the custom field fires an upsert, a tick with
/// no change of any field ships NOTHING (the silent-tick skip that
/// is the delta's entire point). A position-only diff would miss the
/// first half; an always-ship design would waste the second.
#[tokio::test]
async fn delta_diff_fires_on_custom_field_change() {
    let mut r = RichRig::new();

    // Bootstrap (Full) and settle the ledger.
    put_rich(&mut r.s0, 100, -1.0, 3);
    r.step0(1);
    {
        let msgs = r.drain01();
        r.deliver_to_s1(msgs);
    }

    // Quiet tick: no field changed ⇒ NOTHING ships.
    r.step0(2);
    assert!(
        r.drain01().is_empty(),
        "an unchanged strip ships nothing"
    );

    // Change ONLY the custom field: the next tick ships exactly one
    // upsert, carrying the new facing at the unchanged position.
    r.s0.world.ents.get_mut(&100).unwrap().2 = 4;
    r.step0(3);
    let msgs = r.drain01();
    let (_seq, upserts, exits) = expect_delta(&msgs);
    assert_eq!(
        upserts,
        [BorderRecord {
            wire: 100,
            state: TRich {
                x: -1,
                y: 0,
                facing: 4
            }
        }],
        "a custom-field-only change fires the delta"
    );
    assert!(exits.is_empty(), "no exit: the entity never left");
    r.deliver_to_s1(msgs);
    assert_eq!(
        r.s1.border[&0].recs[&100].state.facing, 4,
        "the receiving view took the custom-field update"
    );

    // And quietness resumes once the change was accepted.
    r.step0(4);
    assert!(r.drain01().is_empty(), "no further change ⇒ silent again");
}
