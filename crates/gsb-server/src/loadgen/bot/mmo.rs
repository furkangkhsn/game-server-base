//! The 3D MMO's bot (GAME-MODULE §6 decision 8): characters roam around
//! a waystone, now and then `Travel` to another one (another shard) and
//! `Attack` the mobs that come within reach.
//!
//! **Where a bot starts** (K4, `docs/GAME-MODULE.md`): the MMO places a
//! login by the identity it authenticated as, and the loadgen hosts the
//! MMO over its bots' roster ([`roster`]): bot `id` logs in as
//! `lg-{id}` and finds its saved character on the roaming ring of
//! waystone `id mod 4` — the population starts spread over the four
//! shards, no dispersal `Travel` needed (G3's bots sent one first, when
//! every session started unsaved on shard 0). Against a server whose
//! realm does not know the roster (`--addr` at a catalog `gsb-server`)
//! every bot is unsaved and starts on waystone 0: that server has no
//! characters to restore.
//!
//! **Rates** (per input, from `--move-ms`, so they are rates in time):
//!
//! - `Travel` — one per [`TRAVEL_EVERY`] (20 s) on average, to one of
//!   the three other waystones: a cross-shard handoff per character
//!   every 20 s (at 500 characters, 25 migrations/s) — an MMO's
//!   long-distance travel is occasional, and most of the load stays the
//!   AOI stream of walking characters.
//! - `Attack` — one per [`ATTACK_EVERY`] (1 s) on average while a mob is
//!   within the game's reach ([`ATTACK_RANGE`], 3D): a plain melee
//!   cadence. The standard realm keeps two mobs by every waystone (a camp
//!   and a patrolling pack) and a flyer overhead; the roaming ring below
//!   passes within reach of both ground mobs, so the attacks (and the
//!   combat timer they set) come from ordinary movement.
//! - otherwise `MoveTo`: the target circles the current waystone at
//!   30–60 m (by id), at 0.15 rad/s (4.5–9 m/s: about the characters'
//!   7 m/s run), starting at the golden angle times the id so the
//!   population spreads round the ring. The ring crosses the 64 m AOI
//!   cells around the waystone, so cell exits and one-shot fulls flow.
//!
//! The draws are a per-client SplitMix64 stream seeded by the id: a run
//! is reproducible, and a partitioned (orchestrated) run makes the same
//! draws as an in-process one.
//!
//! **Duelists** (`--mmo-duel-frac`, default none): a fraction of the bots
//! fight across a shard seam instead of roaming — [`duel`].

use std::f64::consts::TAU;
use std::time::Duration;

use gsb_demo_mmo::codec::to_dm;
use gsb_demo_mmo::mmo::{Attack, CellExit, Kind, MoveTo, Travel};
use gsb_demo_mmo::op;
use gsb_demo_mmo::world::{ATTACK_RANGE, SHARDS, WAYSTONES, client_cell};
use gsb_kit::client::wire::{Fields, Value, sint32};
use gsb_kit::client::{ClientDecoder, ClientError, ClientView, Counters, PrivateEvent, Snapshot};
use gsb_server::games::mmo::DEFAULT_WAYSTONE;
use prost::Message;

use super::{BotClient, Labels, LoadBot};

mod duel;
pub(crate) mod roster;
#[cfg(test)]
mod tests;

/// The mean time between two `Travel`s of one character.
const TRAVEL_EVERY: Duration = Duration::from_secs(20);
/// The mean time between two `Attack`s while a mob is within reach.
const ATTACK_EVERY: Duration = Duration::from_secs(1);
/// The roaming ring's angular speed (rad/s).
const ROAM_RAD_S: f64 = 0.15;

/// The MMO's bot family: the input interval turns the rates above into
/// per-input probabilities.
pub(crate) struct MmoBot {
    pub(crate) move_ms: Duration,
    /// The fraction of duelists (`--mmo-duel-frac`, [`duel`]).
    pub(crate) duel_frac: f64,
}

impl MmoBot {
    /// The chance per input of an event meant to happen once per `every`.
    fn per_input(&self, every: Duration) -> f64 {
        (self.move_ms.as_secs_f64() / every.as_secs_f64()).min(1.0)
    }
}

impl LoadBot for MmoBot {
    fn snapshot_op(&self) -> u16 {
        op::MMO_SNAPSHOT
    }

    fn private_op(&self) -> u16 {
        op::MMO_PRIVATE
    }

    fn client(&self, id: u64) -> Box<dyn BotClient> {
        Box::new(MmoClient {
            id,
            entity: None,
            at: roster::home_waystone(id),
            dispersed: false,
            draws: id ^ 0x5EED_5EED_5EED_5EED,
            p_travel: self.per_input(TRAVEL_EVERY),
            p_attack: self.per_input(ATTACK_EVERY),
            view: ClientView::new(MmoDecoder),
            duel: duel::duel_of(id, self.duel_frac),
        })
    }

    fn flood_input(&self) -> (u16, Vec<u8>) {
        let [x, z] = WAYSTONES[DEFAULT_WAYSTONE];
        let msg = MoveTo {
            x: to_dm(x),
            z: to_dm(z),
            seq: 0,
        };
        (op::MMO_MOVE_TO, msg.encode_to_vec())
    }

    fn churn_input(&self, id: u64, seq: u64) -> (u16, Vec<u8>) {
        // A per-id spot by the churn session's home waystone (where its
        // saved character stands — the churn client logs in as `lg-{id}`
        // too).
        let [x, z] = WAYSTONES[roster::home_waystone(id)];
        let msg = MoveTo {
            x: to_dm(x + (id % 40) as f32 - 20.0),
            z: to_dm(z + (id % 37) as f32 - 18.0),
            seq,
        };
        (op::MMO_MOVE_TO, msg.encode_to_vec())
    }

    fn labels(&self) -> Option<Labels> {
        Some(Labels {
            visibility: "spatial",
            shards: SHARDS as u32,
            profile: if self.duel_frac > 0.0 {
                "roam+duel"
            } else {
                "roam"
            },
        })
    }

    fn shard_spread(&self) -> bool {
        true
    }

    fn describe(&self) -> String {
        let duel = if self.duel_frac > 0.0 {
            format!(
                "; duelists (pairs, fraction {}) post 8 m either side of the x = 0 seam and \
                 Attack across it every ~{}s",
                self.duel_frac,
                ATTACK_EVERY.as_secs()
            )
        } else {
            String::new()
        };
        format!(
            "mmo: bot id logs in as its saved character (roster) on waystone id mod {SHARDS} \
             and roams 30–60 m round it; Travel every ~{}s, Attack every ~{}s while a mob \
             is within {ATTACK_RANGE} m{duel}",
            TRAVEL_EVERY.as_secs(),
            ATTACK_EVERY.as_secs()
        )
    }
}

/// One MMO client.
struct MmoClient {
    id: u64,
    entity: Option<u64>,
    /// The waystone the character roams around (its last `Travel`).
    at: usize,
    /// Whether a duelist's first travel (to its post's waystone) was
    /// decided.
    dispersed: bool,
    /// SplitMix64 state.
    draws: u64,
    p_travel: f64,
    p_attack: f64,
    view: ClientView<MmoDecoder>,
    /// The duelist's post (`None`: the roaming bot).
    duel: Option<duel::Duel>,
}

impl MmoClient {
    /// The next uniform draw in `[0, 1)`.
    fn draw(&mut self) -> f64 {
        self.draws = self.draws.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.draws;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        (z ^ (z >> 31)) as f64 / (u64::MAX as f64 + 1.0)
    }

    /// The nearest mob (or flyer) within reach of `me`, by wire id.
    fn mob_in_reach(&self, me: &MmoRecord) -> Option<u64> {
        let reach = f64::from(ATTACK_RANGE) * 10.0; // decimetres
        self.view
            .iter()
            .filter(|(_, r)| r.kind == Kind::Mob as i32 || r.kind == Kind::Flyer as i32)
            .map(|(id, r)| (id, me.dist_dm(r)))
            .filter(|&(_, d)| d <= reach)
            .min_by(|a, b| a.1.total_cmp(&b.1))
            .map(|(id, _)| id)
    }

    fn travel(&mut self, to: usize, seq: u64) -> (u16, Vec<u8>) {
        self.at = to;
        let msg = Travel {
            waystone: to as u32,
            seq,
        };
        (op::MMO_TRAVEL, msg.encode_to_vec())
    }

    /// A duelist's input: first to its side's waystone, then an attack
    /// across the seam when a foe is in reach (at the attack rate), else
    /// the walk to its post.
    fn duel_input(&mut self, me: &MmoRecord, post: duel::Duel, seq: u64) -> (u16, Vec<u8>) {
        if !self.dispersed {
            self.dispersed = true;
            if post.waystone != self.at {
                return self.travel(post.waystone, seq);
            }
        }
        if let Some(target) = duel::foe_in_reach(me, self.view.iter())
            && self.draw() < self.p_attack
        {
            return (op::MMO_ATTACK, Attack { target, seq }.encode_to_vec());
        }
        let (x, z) = (to_dm(post.x), to_dm(post.z));
        (op::MMO_MOVE_TO, MoveTo { x, z, seq }.encode_to_vec())
    }

    /// The roaming ring's target at `elapsed`, decimetres.
    fn roam(&self, elapsed: Duration) -> (i32, i32) {
        let [x, z] = ring(self.at, self.id, elapsed);
        (to_dm(x), to_dm(z))
    }
}

/// Bot `id`'s point on the roaming ring round waystone `at`, `elapsed`
/// into its run, metres: 30–60 m out (by id), at [`ROAM_RAD_S`], starting
/// at the golden angle times the id. The roster saves each character at
/// its ring's start ([`roster::home`]).
fn ring(at: usize, id: u64, elapsed: Duration) -> [f32; 2] {
    let [wx, wz] = WAYSTONES[at];
    let radius = 30.0 + (id % 7) as f64 * 5.0;
    let angle = elapsed.as_secs_f64() * ROAM_RAD_S + id as f64 * (TAU * 0.381_966);
    [
        wx + (radius * angle.cos()) as f32,
        wz + (radius * angle.sin()) as f32,
    ]
}

impl BotClient for MmoClient {
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
        // Nothing until the own character is in view (joined, and — after
        // a travel — handed over to its new shard).
        let me = *self.view.get(self.entity?)?;
        if let Some(post) = self.duel {
            return Some(self.duel_input(&me, post, seq));
        }
        if self.draw() < self.p_travel {
            let to = (self.at + 1 + (self.draw() * 3.0) as usize % 3) % SHARDS;
            return Some(self.travel(to, seq));
        }
        if let Some(target) = self.mob_in_reach(&me)
            && self.draw() < self.p_attack
        {
            let msg = Attack { target, seq };
            return Some((op::MMO_ATTACK, msg.encode_to_vec()));
        }
        let (x, z) = self.roam(elapsed);
        Some((op::MMO_MOVE_TO, MoveTo { x, z, seq }.encode_to_vec()))
    }
}

/// What the MMO bot keeps per entity: the position (decimetres) and the
/// kind (`mmo.proto`'s `EntityRecord` without the hit points).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct MmoRecord {
    pub(crate) x: i32,
    pub(crate) y: i32,
    pub(crate) z: i32,
    pub(crate) kind: i32,
}

impl MmoRecord {
    /// The 3D distance to `other`, decimetres.
    fn dist_dm(&self, other: &MmoRecord) -> f64 {
        let d = |a: i32, b: i32| f64::from(a) - f64::from(b);
        let (dx, dy, dz) = (d(self.x, other.x), d(self.y, other.y), d(self.z, other.z));
        (dx * dx + dy * dy + dz * dz).sqrt()
    }
}

/// The MMO's decode seam: a record walked by hand (`EntityRecord {
/// uint64 entity = 1; sint32 x = 2; sint32 y = 3; sint32 z = 4; Kind
/// kind = 5; uint32 hp = 6; }` — pinned to the generated decoder by
/// this module's tests), in its ground cell (`client_cell`: height is
/// not part of the cell); the rare cell exit (`CellExit { x, z }`, the
/// cell's index) uses the generated type.
pub(crate) struct MmoDecoder;

impl ClientDecoder for MmoDecoder {
    type Record = MmoRecord;
    type Cell = (i32, i32);

    #[inline]
    fn record(&self, body: &[u8]) -> Result<(u64, MmoRecord), ClientError> {
        let (mut entity, mut r) = (0, MmoRecord::default());
        for field in Fields::new(body) {
            match field? {
                (1, Value::Varint(v)) => entity = v,
                (2, Value::Varint(v)) => r.x = sint32(v),
                (3, Value::Varint(v)) => r.y = sint32(v),
                (4, Value::Varint(v)) => r.z = sint32(v),
                // An enum is an `int32` on the wire: the low 32 bits.
                (5, Value::Varint(v)) => r.kind = v as i32,
                (6, Value::Varint(_)) => {}
                (1..=6, _) => return Err(ClientError::Malformed("wrong wire type")),
                _ => {}
            }
        }
        Ok((entity, r))
    }

    #[inline]
    fn cell_of(&self, r: &MmoRecord) -> (i32, i32) {
        client_cell(r.x, r.z)
    }

    fn cell_exit(&self, body: &[u8]) -> Result<(i32, i32), ClientError> {
        let c = CellExit::decode(body)?;
        Ok((c.x, c.z))
    }
}
