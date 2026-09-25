//! One room of the sharded twin: its four shard logics stepped in the
//! core's phase order (the parent module's docs).

use std::collections::HashMap;
use std::time::Duration;

use bevy_ecs::prelude::World;
use bytes::{Bytes, BytesMut};
use gsb_core::id::{ConnectionId, PlayerId, RoomId};
use gsb_core::room::{Action, TickCtx};
use gsb_core::shard::{BorderRecord, Migrating, ShardLogic, TeamExport, TeamImport, TeamImports};

use super::super::game::parse;
use super::super::partition;
use super::{Key, Mig};
use crate::space::Partition;
use crate::testing::{Position, WirePos};

/// Keep-alive cadence, steps.
const KEEP: u64 = 10;

pub(super) type Logic<G, St> =
    Box<dyn ShardLogic<World, GroupKey = G, State = St, Strip = WirePos>>;

/// One room: its shards, their group-frame caches, the exports and
/// crossings of the last tick, and where each player is.
pub(super) struct Room<G, St> {
    shards: Vec<(World, Logic<G, St>)>,
    caches: Vec<HashMap<G, Bytes>>,
    exports: Vec<Option<TeamExport>>,
    crossings: Vec<(usize, usize, Migrating<St>)>,
    /// Each pair's player and the shard it is on (script order).
    pub(super) players: Vec<(PlayerId, usize)>,
    inputs: Vec<Vec<Action>>,
}

/// One tick's frames of a room: per shard, per group, the frame sent.
pub(super) type Sent<G> = Vec<HashMap<G, Bytes>>;

impl<G: Key, St: Mig> Room<G, St> {
    pub(super) fn new(shards: Vec<Logic<G, St>>) -> Self {
        let n = shards.len();
        Self {
            shards: shards.into_iter().map(|l| (World::new(), l)).collect(),
            caches: (0..n).map(|_| HashMap::new()).collect(),
            exports: (0..n).map(|_| None).collect(),
            crossings: Vec::new(),
            players: Vec::new(),
            inputs: (0..n).map(|_| Vec::new()).collect(),
        }
    }

    pub(super) fn join(&mut self, conn: u64, identity: &str) {
        let (x, y, _) = parse(identity);
        let home = Partition::<WirePos>::region_of(&partition(), &Position { x, y });
        let (world, logic) = &mut self.shards[home];
        let admitted = logic.on_join_as(world, ConnectionId(conn), identity);
        self.players.push((admitted.player, home));
    }

    pub(super) fn leave(&mut self, i: usize) {
        let (player, at) = self.players.remove(i);
        let (world, logic) = &mut self.shards[at];
        logic.on_leave(world, player);
        self.crossings.retain(|(_, _, m)| m.player != Some(player));
    }

    pub(super) fn input(&mut self, i: usize, op: u16, payload: Bytes) {
        let (player, at) = self.players[i];
        self.inputs[at].push(Action {
            conn: ConnectionId(0),
            player,
            op,
            payload,
        });
    }

    /// One step in the core's phase order (module docs).
    pub(super) fn step(&mut self, tick: u64) -> Sent<G> {
        let ctx = TickCtx {
            room: RoomId(1),
            tick,
            dt: Duration::from_secs_f64(1.0 / 30.0),
            idle: Default::default(),
        };
        for (from, to, m) in std::mem::take(&mut self.crossings) {
            let (world, logic) = &mut self.shards[from];
            logic.on_migrate_out(world, m.wire);
            let (world, logic) = &mut self.shards[to];
            logic.on_migrate_in(world, m.wire, m.state, m.player);
            if let Some(p) = self.players.iter_mut().find(|(p, _)| Some(*p) == m.player) {
                p.1 = to;
            }
        }
        for (i, (world, logic)) in self.shards.iter_mut().enumerate() {
            logic.ingest(world, &ctx, &mut self.inputs[i]);
            logic.update(world, &ctx);
        }
        for (i, (world, logic)) in self.shards.iter_mut().enumerate() {
            for j in logic.neighbors().to_vec() {
                for m in logic.collect_migrations(world, j) {
                    self.crossings.push((i, j, m));
                }
            }
        }
        let strips: Vec<Vec<BorderRecord<WirePos>>> = self
            .shards
            .iter()
            .map(|(w, l)| l.collect_border(w))
            .collect();
        let mut borrowed = Vec::new();
        let mut exports = Vec::new();
        for (j, (world, logic)) in self.shards.iter_mut().enumerate() {
            let own = logic.own_wires(world);
            let strip: Vec<BorderRecord<WirePos>> = (0..strips.len())
                .filter(|&i| i != j)
                .flat_map(|i| strips[i].iter().cloned())
                .filter(|r| !own.contains(&r.wire))
                .collect();
            let mut imports = TeamImports::default();
            for (i, export) in self.exports.iter().enumerate() {
                if let (true, Some(e)) = (i != j, export) {
                    imports.insert(TeamImport {
                        from: i,
                        tick: tick - 1,
                        records: e.records.clone(),
                    });
                }
            }
            imports.settle();
            exports.push(logic.team_exchange(world, &ctx, &strip, &imports));
            borrowed.push(strip);
        }
        self.exports = exports;
        self.broadcast(&ctx, &borrowed)
    }

    fn broadcast(&mut self, ctx: &TickCtx, borrowed: &[Vec<BorderRecord<WirePos>>]) -> Sent<G> {
        let mut sent = Vec::new();
        for (j, (world, logic)) in self.shards.iter_mut().enumerate() {
            let mut groups: Vec<G> = Vec::new();
            for &(p, at) in &self.players {
                let g = logic.group_of(world, p);
                if at == j && !groups.contains(&g) {
                    groups.push(g);
                }
            }
            let cache = &mut self.caches[j];
            cache.retain(|g, _| groups.contains(g));
            let mut frames = HashMap::new();
            for g in groups {
                let mut out = BytesMut::new();
                if logic.snapshot(world, ctx, &g, &borrowed[j], &mut out) {
                    let frame = out.split().freeze();
                    cache.insert(g.clone(), frame.clone());
                    frames.insert(g.clone(), frame);
                }
                if ctx.tick.is_multiple_of(KEEP) && cache.contains_key(&g) {
                    if logic.keepalive(world, ctx, &g, cache.get(&g), &mut out) {
                        cache.insert(g.clone(), out.split().freeze());
                    }
                    frames.insert(g.clone(), cache[&g].clone());
                }
            }
            sent.push(frames);
        }
        sent
    }

    /// Player `i`'s frames this tick: its group's, then its private one.
    pub(super) fn batch(&mut self, sent: &Sent<G>, i: usize) -> Vec<(bool, Bytes)> {
        let (p, at) = self.players[i];
        let (world, logic) = &mut self.shards[at];
        let g = logic.group_of(world, p);
        let mut batch = Vec::new();
        if let Some(frame) = sent[at].get(&g) {
            batch.push((true, frame.clone()));
        }
        let mut out = BytesMut::new();
        if logic.private(world, p, &g, &[], &mut out) {
            batch.push((false, out.freeze()));
        }
        batch
    }
}
