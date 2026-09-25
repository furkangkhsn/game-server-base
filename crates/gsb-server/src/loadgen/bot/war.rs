//! The war's bot ("Cephe", W2): characters of the three factions hold
//! POSTS — the twelve watchtowers and the two capture points — walking a
//! ring round their post, now and then moving on to a post nearby, and
//! striking the nearest enemy that comes within reach.
//!
//! **Where a bot starts** (K4): the load generator hosts the war over its
//! bots' roster ([`roster`]): bot `id` logs in as `lg-{id}` and finds its
//! saved character — faction `id mod 3` — on the ring of post
//! `(id / 3) mod 14`: every post is held by all three factions, and the
//! population starts spread over the four shards (three posts in each
//! region, two more in the contested one). A bot beyond the roster, or
//! against a catalog server, is unsaved: its faction comes from the
//! war's identity hash and it starts at that faction's base.
//!
//! **The faction comes from the wire**: the session's first private
//! frame carries the war's `Welcome`; until it — and the own unit — are
//! in view the bot sends nothing. A record's `faction` tells ally from
//! enemy.
//!
//! **Rates** (per input, from `--move-ms`, so they are rates in time):
//!
//! - `Attack` — one per [`ATTACK_EVERY`] (5 s) on average while an enemy
//!   player is within the game's reach (20 m on the ground): four blows
//!   fell a player, who respawns at its base and walks back to its post
//!   — a melee war, but not a massacre that leaves every post empty.
//! - moving on — one per [`MOVE_ON_EVERY`] (40 s) on average, to one of
//!   the three posts nearest the current one (300–500 m: two other
//!   towers of the region, or across a seam);
//! - otherwise `MoveTo`: the ring round the post, 20–45 m out (by id) —
//!   inside the 60 m vision, so a post's defenders see each other — at
//!   0.15 rad/s, starting at the golden angle times the id.
//!
//! The draws are a per-client SplitMix64 stream seeded by the id: a run
//! is reproducible, and an orchestrated run makes the same draws as an
//! in-process one.

use std::f64::consts::TAU;
use std::sync::OnceLock;
use std::time::Duration;

use gsb_demo_war::codec::to_dm;
use gsb_demo_war::op;
use gsb_demo_war::war::{Attack, Kind, MoveTo, Welcome};
use gsb_demo_war::world::{ATTACK_RANGE, FACTIONS, POINTS, SHARDS, tower};
use gsb_kit::client::wire::{Fields, Value, sint32};
use gsb_kit::client::{ClientDecoder, ClientError, ClientView, Counters, PrivateEvent, Snapshot};
use gsb_kit::team::Team;
use prost::Message;

use super::{BotClient, Labels, LoadBot};

pub(crate) mod roster;
#[cfg(test)]
mod tests;

/// The mean time between two attacks while an enemy is within reach.
const ATTACK_EVERY: Duration = Duration::from_secs(5);
/// The mean time between two moves to another post.
const MOVE_ON_EVERY: Duration = Duration::from_secs(40);
/// The ring's angular speed (rad/s).
const RING_RAD_S: f64 = 0.15;

/// The posts, ground `(x, z)` metres: every tower (faction-major, region
/// order), then the capture points. Computed once.
pub(crate) fn posts() -> &'static [[f32; 2]] {
    static POSTS: OnceLock<Vec<[f32; 2]>> = OnceLock::new();
    POSTS.get_or_init(|| {
        let towers = (0..FACTIONS).flat_map(|f| (0..SHARDS).map(move |r| tower(Team(f), r)));
        towers.chain(POINTS).collect()
    })
}

/// The three posts nearest post `at` (not itself), nearest first.
pub(crate) fn nearest_posts(at: usize) -> [usize; 3] {
    let all = posts();
    let d2 = |i: usize| {
        let (a, b) = (all[at], all[i]);
        (a[0] - b[0]).powi(2) + (a[1] - b[1]).powi(2)
    };
    let mut others: Vec<usize> = (0..all.len()).filter(|&i| i != at).collect();
    others.sort_by(|&a, &b| d2(a).total_cmp(&d2(b)));
    [others[0], others[1], others[2]]
}

/// Bot `id`'s point on the ring round post `post`, `elapsed` into its
/// run, metres (module docs).
pub(crate) fn ring(post: usize, id: u64, elapsed: Duration) -> [f32; 2] {
    let [px, pz] = posts()[post];
    let radius = 20.0 + (id % 6) as f64 * 5.0;
    let angle = elapsed.as_secs_f64() * RING_RAD_S + id as f64 * (TAU * 0.381_966);
    [
        px + (radius * angle.cos()) as f32,
        pz + (radius * angle.sin()) as f32,
    ]
}

/// The war's bot family.
pub(crate) struct WarBot {
    pub(crate) move_ms: Duration,
}

impl WarBot {
    /// The chance per input of an event meant to happen once per `every`.
    fn per_input(&self, every: Duration) -> f64 {
        (self.move_ms.as_secs_f64() / every.as_secs_f64()).min(1.0)
    }
}

impl LoadBot for WarBot {
    fn snapshot_op(&self) -> u16 {
        op::WAR_SNAPSHOT
    }

    fn private_op(&self) -> u16 {
        op::WAR_PRIVATE
    }

    fn client(&self, id: u64) -> Box<dyn BotClient> {
        Box::new(WarClient {
            id,
            entity: None,
            post: roster::home_post(id),
            draws: id ^ 0xC3F3_C3F3_C3F3_C3F3,
            p_attack: self.per_input(ATTACK_EVERY),
            p_move_on: self.per_input(MOVE_ON_EVERY),
            view: ClientView::new(WarDecoder::default()),
        })
    }

    fn flood_input(&self) -> (u16, Vec<u8>) {
        (
            op::WAR_MOVE_TO,
            MoveTo { x: 0, z: 0, seq: 0 }.encode_to_vec(),
        )
    }

    fn churn_input(&self, id: u64, seq: u64) -> (u16, Vec<u8>) {
        // A per-id spot on the churn session's home ring (it logs in as
        // `lg-{id}` too, but keeps no view).
        let [x, z] = ring(roster::home_post(id), id, Duration::ZERO);
        let msg = MoveTo {
            x: to_dm(x + (id % 7) as f32),
            z: to_dm(z),
            seq,
        };
        (op::WAR_MOVE_TO, msg.encode_to_vec())
    }

    fn labels(&self) -> Option<Labels> {
        Some(Labels {
            visibility: "team",
            shards: SHARDS as u32,
            profile: "posts",
        })
    }

    fn shard_spread(&self) -> bool {
        true
    }

    fn team_relay(&self) -> bool {
        true
    }

    fn describe(&self) -> String {
        format!(
            "war: bot id logs in as its saved character (roster) — faction id mod {FACTIONS}, \
             on post (id / 3) mod {} (the towers and capture points); rings 20–45 m round \
             it, moves on to a nearby post every ~{}s, Attacks the nearest enemy within \
             {ATTACK_RANGE} m every ~{}s; faction from the Welcome",
            posts().len(),
            MOVE_ON_EVERY.as_secs(),
            ATTACK_EVERY.as_secs()
        )
    }
}

/// One war client.
struct WarClient {
    id: u64,
    entity: Option<u64>,
    /// The post the character holds.
    post: usize,
    /// SplitMix64 state.
    draws: u64,
    p_attack: f64,
    p_move_on: f64,
    view: ClientView<WarDecoder>,
}

impl WarClient {
    /// The next uniform draw in `[0, 1)`.
    fn draw(&mut self) -> f64 {
        self.draws = self.draws.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.draws;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        (z ^ (z >> 31)) as f64 / (u64::MAX as f64 + 1.0)
    }

    /// The nearest standing enemy player within reach of `me`.
    fn enemy_in_reach(&self, me: &WarRecord, faction: u32) -> Option<u64> {
        let reach = f64::from(ATTACK_RANGE) * 10.0; // decimetres
        self.view
            .iter()
            .filter(|(_, r)| r.kind == Kind::Player as i32 && r.faction != faction && r.hp > 0)
            .map(|(id, r)| (id, me.dist_dm(r)))
            .filter(|&(_, d)| d <= reach)
            .min_by(|a, b| a.1.total_cmp(&b.1))
            .map(|(id, _)| id)
    }
}

impl BotClient for WarClient {
    fn apply_snapshot(&mut self, frame: &[u8]) -> Result<Snapshot, ClientError> {
        self.view.apply_snapshot(frame)
    }

    fn apply_private(&mut self, frame: &[u8]) -> Result<PrivateEvent, ClientError> {
        self.view.apply_private(frame)
    }

    fn counters(&self) -> Counters {
        *self.view.counters()
    }

    fn view_len(&self) -> usize {
        self.view.len()
    }

    fn joined(&mut self, entity: u64) {
        self.entity = Some(entity);
    }

    fn next_input(&mut self, elapsed: Duration, seq: u64) -> Option<(u16, Vec<u8>)> {
        // Nothing until welcomed and the own unit is in view.
        let faction = self.view.decoder().faction?;
        let me = *self.view.get(self.entity?)?;
        if let Some(target) = self.enemy_in_reach(&me, faction)
            && self.draw() < self.p_attack
        {
            return Some((op::WAR_ATTACK, Attack { target, seq }.encode_to_vec()));
        }
        if self.draw() < self.p_move_on {
            let near = nearest_posts(self.post);
            self.post = near[(self.draw() * 3.0) as usize % 3];
        }
        let [x, z] = ring(self.post, self.id, elapsed);
        let msg = MoveTo {
            x: to_dm(x),
            z: to_dm(z),
            seq,
        };
        Some((op::WAR_MOVE_TO, msg.encode_to_vec()))
    }
}

/// What the war bot keeps per unit (`war.proto`'s `UnitRecord` without
/// the height): ground position (decimetres), kind, faction (1-based),
/// hit points.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct WarRecord {
    pub(crate) x: i32,
    pub(crate) z: i32,
    pub(crate) kind: i32,
    pub(crate) faction: u32,
    pub(crate) hp: u32,
}

impl WarRecord {
    /// The ground distance to `other`, decimetres.
    fn dist_dm(&self, other: &WarRecord) -> f64 {
        let (dx, dz) = (
            f64::from(self.x) - f64::from(other.x),
            f64::from(self.z) - f64::from(other.z),
        );
        (dx * dx + dz * dz).sqrt()
    }
}

/// The war's decode seam: a record walked by hand (`UnitRecord { uint64
/// entity = 1; sint32 x = 2; sint32 y = 3; sint32 z = 4; Kind kind = 5;
/// uint32 faction = 6; uint32 hp = 7; }` — pinned to the generated
/// decoder by this module's tests); team frames carry no cell exits; the
/// session payload is the `Welcome` (decoded with the generated type —
/// once per session), whose faction the decoder keeps.
#[derive(Default)]
pub(crate) struct WarDecoder {
    /// The own faction, 1-based (`None` until welcomed).
    faction: Option<u32>,
}

impl ClientDecoder for WarDecoder {
    type Record = WarRecord;
    type Cell = ();

    #[inline]
    fn record(&self, body: &[u8]) -> Result<(u64, WarRecord), ClientError> {
        let (mut entity, mut r) = (0, WarRecord::default());
        for field in Fields::new(body) {
            match field? {
                (1, Value::Varint(v)) => entity = v,
                (2, Value::Varint(v)) => r.x = sint32(v),
                (3, Value::Varint(_)) => {}
                (4, Value::Varint(v)) => r.z = sint32(v),
                // An enum is an `int32` on the wire: the low 32 bits.
                (5, Value::Varint(v)) => r.kind = v as i32,
                (6, Value::Varint(v)) => r.faction = v as u32,
                (7, Value::Varint(v)) => r.hp = v as u32,
                (1..=7, _) => return Err(ClientError::Malformed("wrong wire type")),
                _ => {}
            }
        }
        Ok((entity, r))
    }

    #[inline]
    fn cell_of(&self, _: &WarRecord) {}

    fn cell_exit(&self, _: &[u8]) -> Result<(), ClientError> {
        Err(ClientError::Malformed("the war sends no cell exits"))
    }

    fn session_private(&mut self, body: &[u8]) -> Result<(), ClientError> {
        let Welcome { faction, factions } = Welcome::decode(body)?;
        if faction == 0 || faction > factions {
            return Err(ClientError::Malformed("a faction out of range"));
        }
        self.faction = Some(faction);
        Ok(())
    }
}
