//! The realm's data — what a live MMO loads from its content and
//! character databases, and what each shard's game instance takes its
//! own slice of: the saved characters (where a login appears) and the
//! mob spawn table (the camps GAME code spawns mobs from).

use std::collections::HashMap;

use gsb_core::id::ConnectionId;

use crate::components::{Kind, Pos3};
use crate::world::{WAYSTONES, home_shard};

/// One row of the spawn table: a camp that spawns a mob of `kind` at
/// `at` on tick `first`, then every `every` ticks (a respawning camp) or
/// once (`None`). Each mob walks `route` at `pace` and despawns
/// `lifetime` ticks after its spawn — all of it game code.
#[derive(Debug, Clone, PartialEq)]
pub struct MobSpawn {
    pub kind: Kind,
    /// Spawn point (a flyer's `y` is its altitude).
    pub at: Pos3,
    /// Ground waypoints `(x, z)`; empty = the mob stands at its spawn.
    pub route: Vec<[f32; 2]>,
    /// Metres per second along the route.
    pub pace: f32,
    /// Loop the route.
    pub patrol: bool,
    pub hp: u16,
    /// The first spawn's global tick.
    pub first: u64,
    /// Respawn period in ticks (`None` = spawn once).
    pub every: Option<u64>,
    /// Ticks from spawn to despawn.
    pub lifetime: u64,
}

impl MobSpawn {
    /// A single mob of `kind` standing at `at` from tick `first`,
    /// living `lifetime` ticks, with `hp` hit points.
    #[must_use]
    pub fn once(kind: Kind, at: Pos3, first: u64, lifetime: u64, hp: u16) -> Self {
        Self {
            kind,
            at,
            route: Vec::new(),
            pace: 0.0,
            patrol: false,
            hp,
            first,
            every: None,
            lifetime,
        }
    }

    /// The same spawn walking `route` at `pace` m/s.
    #[must_use]
    pub fn walking(mut self, route: Vec<[f32; 2]>, pace: f32, patrol: bool) -> Self {
        self.route = route;
        self.pace = pace;
        self.patrol = patrol;
        self
    }
}

/// The realm: saved characters and the spawn table. Every shard's game
/// instance is built from the same realm and keeps what is its own.
#[derive(Debug, Clone, Default)]
pub struct Realm {
    /// Saved character positions, keyed by the session the login
    /// arrives on. (The kit hands `spawn_player` the transport session
    /// only — the account identity stops at the core's `on_join`; the
    /// server's login step would fill this table before the join.)
    pub logins: HashMap<ConnectionId, Pos3>,
    /// The mob spawn table.
    pub spawns: Vec<MobSpawn>,
}

impl Realm {
    /// A realm with no characters and no mobs (tests script their own).
    #[must_use]
    pub fn empty() -> Self {
        Self::default()
    }

    /// The live realm's spawn table: in every region a ground camp by
    /// its waystone, a wolf pack patrolling across the seam to the east
    /// or west, and a flyer circling over the map's centre (it crosses
    /// all four regions every loop).
    #[must_use]
    pub fn standard() -> Self {
        let mut spawns = Vec::new();
        for (i, [wx, wz]) in WAYSTONES.into_iter().enumerate() {
            let camp = Pos3::new(wx + 40.0, 0.0, wz + 40.0);
            spawns.push(MobSpawn {
                every: Some(600),
                ..MobSpawn::once(Kind::Mob, camp, 30, 1_800, 100)
            });
            let east = if i % 2 == 0 { 1.0 } else { -1.0 };
            let pack = Pos3::new(wx, 0.0, wz - 60.0);
            spawns.push(
                MobSpawn {
                    every: Some(900),
                    ..MobSpawn::once(Kind::Mob, pack, 60, 2_700, 150)
                }
                .walking(
                    vec![[wx + east * 320.0, wz - 60.0], [wx, wz - 60.0]],
                    2.0,
                    true,
                ),
            );
        }
        let ring = vec![[80.0, 80.0], [-80.0, 80.0], [-80.0, -80.0], [80.0, -80.0]];
        spawns.push(
            MobSpawn {
                every: Some(1_200),
                ..MobSpawn::once(Kind::Flyer, Pos3::new(80.0, 120.0, -80.0), 90, 3_600, 60)
            }
            .walking(ring, 6.0, true),
        );
        Self {
            logins: HashMap::new(),
            spawns,
        }
    }

    /// Save a character at `pos` for the login on `conn`.
    #[must_use]
    pub fn with_login(mut self, conn: u64, pos: Pos3) -> Self {
        self.logins.insert(ConnectionId(conn), pos);
        self
    }

    /// Add a spawn-table row.
    #[must_use]
    pub fn with_spawn(mut self, spawn: MobSpawn) -> Self {
        self.spawns.push(spawn);
        self
    }

    /// The spawn-table rows whose spawn point lies in shard `index`'s
    /// region (a shard spawns only on its own ground).
    pub fn spawns_of(&self, index: usize) -> impl Iterator<Item = &MobSpawn> {
        self.spawns
            .iter()
            .filter(move |s| home_shard(&s.at) == index)
    }
}
