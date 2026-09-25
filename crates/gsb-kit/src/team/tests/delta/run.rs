//! A seeded random team-fog world, replayable on several rooms at once:
//! the script draws each tick's operations as data (joins, leaves, walks
//! and teleports, runtime team changes, neutral spawns, moves and
//! despawns — three teams, a 25 radius on a 160-wide floor so units
//! keep entering and leaving each other's vision), and every [`Twin`]
//! applies the same list to its own world and room.

use bevy_ecs::prelude::Entity;

use super::*;

/// SplitMix64: a fixed seed gives the same run everywhere.
pub(super) struct Rng(u64);

impl Rng {
    pub(super) fn new(seed: u64) -> Self {
        Self(seed)
    }

    pub(super) fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// `true` with probability `p`.
    pub(super) fn chance(&mut self, p: f64) -> bool {
        (self.next() >> 11) as f64 / (1u64 << 53) as f64 <= p
    }

    /// Uniform in `0..n` (`n > 0`).
    pub(super) fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }

    /// Uniform in `[-half, half)`.
    pub(super) fn coord(&mut self, half: f32) -> f32 {
        (self.next() % 10_000) as f32 / 10_000.0 * 2.0 * half - half
    }
}

/// One operation; players and neutrals are named by their index in
/// the twins' (identical) live lists.
#[derive(Debug, Clone, Copy)]
pub(super) enum Op {
    Join(u64),
    Leave(usize),
    Place(usize, f32, f32),
    Switch(usize, u8),
    Spawn(f32, f32),
    PlaceNeutral(usize, f32, f32),
    Despawn(usize),
}

/// The script: which players and neutrals are alive, and the draw.
pub(super) struct Script {
    rng: Rng,
    next_conn: u64,
    players: Vec<(f32, f32)>,
    neutrals: usize,
}

impl Script {
    pub(super) fn new(seed: u64) -> Self {
        Self {
            rng: Rng::new(seed),
            next_conn: 1,
            players: Vec::new(),
            neutrals: 0,
        }
    }

    /// This tick's operations.
    pub(super) fn tick(&mut self) -> Vec<Op> {
        let r = &mut self.rng;
        let mut ops = Vec::new();
        if self.players.len() < 24 && r.chance(0.25) {
            ops.push(Op::Join(self.next_conn));
            self.next_conn += 1;
            let at = (r.coord(80.0), r.coord(80.0));
            self.players.push(at);
            ops.push(Op::Place(self.players.len() - 1, at.0, at.1));
        }
        if self.players.len() > 4 && r.chance(0.04) {
            let i = r.below(self.players.len());
            self.players.remove(i);
            ops.push(Op::Leave(i));
        }
        for (i, at) in self.players.iter_mut().enumerate() {
            if r.chance(0.02) {
                *at = (r.coord(80.0), r.coord(80.0)); // a teleport
            } else if r.chance(0.5) {
                at.0 = (at.0 + r.coord(3.0)).clamp(-80.0, 80.0);
                at.1 = (at.1 + r.coord(3.0)).clamp(-80.0, 80.0);
            } else {
                continue; // standing still this tick
            }
            ops.push(Op::Place(i, at.0, at.1));
        }
        if !self.players.is_empty() && r.chance(0.03) {
            ops.push(Op::Switch(
                r.below(self.players.len()),
                (r.next() % 3) as u8,
            ));
        }
        if self.neutrals < 8 && r.chance(0.08) {
            ops.push(Op::Spawn(r.coord(80.0), r.coord(80.0)));
            self.neutrals += 1;
        }
        if self.neutrals > 0 && r.chance(0.1) {
            let i = r.below(self.neutrals);
            ops.push(Op::PlaceNeutral(i, r.coord(80.0), r.coord(80.0)));
        }
        if self.neutrals > 0 && r.chance(0.04) {
            ops.push(Op::Despawn(r.below(self.neutrals)));
            self.neutrals -= 1;
        }
        ops
    }
}

/// One room over its own world, fed the script's operations.
pub(super) struct Twin {
    pub(super) world: World,
    pub(super) room: TeamRoom,
    /// The live players, in the script's order.
    pub(super) players: Vec<PlayerId>,
    neutrals: Vec<Entity>,
}

impl Twin {
    pub(super) fn new(room: TeamRoom) -> Self {
        Self {
            world: World::new(),
            room,
            players: Vec::new(),
            neutrals: Vec::new(),
        }
    }

    /// Apply one operation; returns the player a join admitted.
    pub(super) fn apply(&mut self, op: Op) -> Option<PlayerId> {
        let (world, room) = (&mut self.world, &mut self.room);
        match op {
            Op::Join(conn) => {
                let player = room.on_join(world, ConnectionId(conn)).player;
                self.players.push(player);
                return Some(player);
            }
            Op::Leave(i) => room.on_leave(world, self.players.remove(i)),
            Op::Place(i, x, y) => {
                let entity = room.player_entity[&self.players[i]];
                world.entity_mut(entity).insert(Position { x, y });
            }
            Op::Switch(i, team) => {
                let entity = room.player_entity[&self.players[i]];
                world.entity_mut(entity).insert(TeamMember(Team(team)));
            }
            Op::Spawn(x, y) => self.neutrals.push(world.spawn(Position { x, y }).id()),
            Op::PlaceNeutral(i, x, y) => {
                world.entity_mut(self.neutrals[i]).insert(Position { x, y });
            }
            Op::Despawn(i) => {
                world.despawn(self.neutrals.remove(i));
            }
        }
        None
    }
}
