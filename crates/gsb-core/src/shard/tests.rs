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
use crate::channel::{FrameBatch, Inbox, Mailbox};
use crate::id::{ConnectionId, EntityId, PlayerId, RoomId};
use crate::metrics::MetricsEvent;
use crate::room::GameLogic;
use crate::room::{Action, RoomConfig, TickCtx};
use crate::ticker::TickInfo;
use std::collections::HashMap;
use std::fmt::Debug;
use std::time::Instant;
use tokio::sync::{broadcast, mpsc, oneshot};

mod border;
mod dropped;
mod effects;
mod hold;
mod identity;
mod idle;
mod keepalive;
mod metrics;
mod migration;
mod strip;
mod teams;

mod rigs;
pub(in crate::shard::tests) use rigs::*;

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
    // -- The cross-seam probes (`tests/effects.rs`). --------------------
    /// Effects the logic applied, in application order.
    applied: Vec<RemoteEffect>,
    /// `(target, source)` pairs the next `update_seam` emits.
    script: Vec<(u64, u64)>,
    /// What those emissions answered.
    emits: Vec<Result<EffectId, EmitRefused>>,
    /// What the last `update_seam` saw lent: `(wire, lender)`, sorted.
    seen_lent: Vec<(u64, usize)>,
    /// `(target, CrossSeam::departed(target))` of every scripted
    /// emission, asked just before it.
    departed: Vec<(u64, Option<usize>)>,
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
/// `mode` in x per tick), wire ids interleaved like the kit's
/// (`interleaved_id`, one draw per join).
struct TLogic {
    index: usize,
    next_serial: u64,
    /// The serials this shard may draw (the exhaustion bound).
    capacity: u64,
    player_ent: HashMap<PlayerId, u64>,
    ent_player: HashMap<u64, PlayerId>,
    last_tick: u64,
    obs: mpsc::Sender<Obs>,
    ops: mpsc::Sender<(PlayerId, u16)>,
}

impl TLogic {
    fn region_of(x: f32) -> usize {
        if x < 0.0 { 0 } else { 1 }
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
        let wire = interleaved_id(self.index, 2, self.next_serial);
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
        let mut c: Vec<(u64, f32, f32, i8)> = w
            .ents
            .iter()
            .map(|(wire, e)| (*wire, e.0, e.1, e.2))
            .collect();
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
    fn serial_capacity(&self) -> u64 {
        self.capacity
    }
    fn serial_used(&self) -> u64 {
        self.next_serial
    }
    fn neighbors(&self) -> &[usize] {
        if self.index == 0 { &[1] } else { &[0] }
    }
    fn collect_migrations(&mut self, w: &mut TWorld, neighbor: usize) -> Vec<Migrating<TState>> {
        let mut out = Vec::new();
        for (&wire, (x, y, mode)) in &w.ents {
            if TLogic::region_of(*x) == neighbor {
                let _ = self
                    .obs
                    .try_send(Obs::Migrate(self.index, self.last_tick, wire));
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
    fn update_seam(&mut self, w: &mut TWorld, ctx: &TickCtx, seam: &mut CrossSeam<'_, TStrip>) {
        self.update(w, ctx);
        let mut seen: Vec<(u64, usize)> = seam.iter().map(|l| (l.wire, l.lender)).collect();
        seen.sort_unstable();
        w.seen_lent = seen;
        for (target, source) in std::mem::take(&mut w.script) {
            w.departed.push((target, seam.departed(target)));
            let answer = seam.emit(target, source, bytes::Bytes::from_static(b"hit"));
            w.emits.push(answer);
        }
    }
    fn apply_remote_effect(
        &mut self,
        w: &mut TWorld,
        _tick: u64,
        effect: &RemoteEffect,
        _seam: &mut CrossSeam<'_, TStrip>,
    ) -> EffectOutcome {
        if !w.ents.contains_key(&effect.target) {
            return EffectOutcome::NoTarget;
        }
        w.applied.push(effect.clone());
        EffectOutcome::Applied
    }
}
