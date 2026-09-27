//! The `team × sharded` composite ([`ShardedTeamRoom`] —
//! `docs/CROSS-SHARD.md` §8b): the grid of [`ShardedRoom`] with team fog
//! over the whole map.
//!
//! ## What a player sees
//!
//! Its team's group, as in [`crate::team::TeamRoom`], but over N shards:
//!
//! - every unit of its team, map-wide — the members on its own shard, and
//!   the other shards' members through the registry hub (each shard
//!   exports its teams' visible sets every tick; the hub relays them to
//!   the shards viewing those teams);
//! - an enemy only if a unit of its team sees it — on this shard (own
//!   entities, and the border strip's lent records when the game maps a
//!   lent wire value to a vision position), or on any other shard (that
//!   shard exported the enemy under the team);
//! - this shard's neutral entities (no `TeamMember`), as the team room
//!   shows them; a neutral elsewhere only through vision, like an enemy.
//!
//! ## One record per wire
//!
//! A wire id can reach a team's content three ways: an own entity, a
//! lent record, an imported record. It is listed ONCE; the payload's
//! precedence is **own > lent > imported** (the freshest typed value),
//! and visibility is the union (an import means "the team sees this
//! wire"). So an enemy of this shard that an ally across the seam sees
//! is shown with this shard's own record — not twice.
//!
//! ## What travels
//!
//! [`TeamMember`] is the kit's component, not the game's state: a
//! migrating entity carries its team in [`TeamMig`] and gets the
//! component back on arrival. A player arriving on a shard has no
//! baseline for that shard's team view: in delta mode its next private
//! frame is the one-shot full (the spatial composite's fresh-member
//! rule).

use std::collections::HashMap;

use bytes::Bytes;

use crate::common::{Baselines, SetLedger};
use crate::game::{ShardGame, TeamGame, Wire};
use crate::sharded::*;
use crate::space::{Partition, Vision};
use crate::team::{Team, TeamMember};

mod budget;
mod content;
mod frames;
mod logic;
mod shard;

use budget::Budget;
pub(crate) use frames::Shown;

/// The default per-team, per-tick export budget of a shard (records):
/// members first, then the enemies its units see; the rest is cut and
/// counted ([`ShardedTeamRoom::over_budget`]). Which records a cut keeps
/// within each tier: [`ShardedTeamRoom::with_export_rank`].
pub const DEFAULT_TEAM_BUDGET: usize = 1024;

/// What a migrating entity carries in the team composite: the sharded
/// room's state plus the entity's team ([`TeamMember`] is written back
/// on arrival).
#[derive(Debug, Clone)]
pub struct TeamMig<M> {
    /// The sharded room's migration state (the game's capture and the
    /// kit's records).
    pub kit: KitMig<M>,
    /// The entity's team (`None`: a neutral entity).
    pub team: Option<Team>,
}

/// The `team × sharded` composite (module docs). `G` is the game (it
/// shards and assigns teams), `P` the map partition, `V` the vision
/// model.
///
/// Composition, not duplication: the grid protocol (minting, migration,
/// park ledger, border cache, cross-seam hooks, RPC) lives in the wrapped
/// [`ShardedRoom`]; this type adds the team surface — the group key, the
/// per-team content built each tick from own + lent + imported records,
/// the export, and the team room's two snapshot modes over the shared
/// set ledger.
pub struct ShardedTeamRoom<G, P, V>
where
    G: ShardGame + TeamGame,
    P: Partition<Wire<G>>,
    V: Vision,
{
    /// The grid-protocol half.
    pub(in crate::sharded) inner: ShardedRoom<G, P>,
    /// The vision model.
    pub(in crate::sharded) vision: V,
    /// Where a LENT record (a neighbour's border strip) stands for
    /// vision: the strip carries only the wire value, so the game maps
    /// it (`None`: the record takes no part in vision here).
    pub(in crate::sharded) lent_pos: fn(&Wire<G>) -> Option<V::Pos>,
    /// The per-team export budget and the game's rank (`budget`).
    pub(in crate::sharded) budget: Budget<Wire<G>>,
    /// Records the budget cut, cumulative.
    pub(in crate::sharded) over_budget: u64,
    /// Delta mode ([`Self::with_delta`]).
    pub(in crate::sharded) delta: bool,
    /// Per team (indexed by the team number): this tick's content —
    /// `wire id → shown value` — built in the TEAMS phase for the teams
    /// with viewers here.
    pub(in crate::sharded) contents: Vec<HashMap<u64, Shown<Wire<G>>>>,
    /// Per team: the set ledger (full mode's "no change", delta mode's
    /// baseline).
    pub(in crate::sharded) ledgers: Vec<SetLedger<Shown<Wire<G>>>>,
    /// Delta mode: which team's view each player's session holds.
    pub(in crate::sharded) baselines: Baselines<Team>,
    /// The export's record bodies by wire id: `(wire value, body, tick
    /// last exported)` — re-encoded only when the value changes, dropped
    /// when not exported for a tick.
    pub(in crate::sharded) bodies: HashMap<u64, (Wire<G>, Bytes, u64)>,
    /// The room's step counter (the ledgers' "previous step").
    pub(in crate::sharded) step: u64,
    /// The global tick of the current step.
    pub(in crate::sharded) tick: u64,
    /// Records encoded in the latest broadcast phase.
    pub(in crate::sharded) encoded: u64,
}

impl<G, P, V> ShardedTeamRoom<G, P, V>
where
    G: ShardGame + TeamGame,
    P: Partition<Wire<G>>,
    V: Vision,
{
    /// Build the team composite around the shard `inner` (see
    /// [`ShardedRoom::with_game`]), with the vision model `vision` and
    /// the game's map from a lent wire value to a vision position
    /// (`lent_pos`; module docs, "What a player sees").
    pub fn with_shard(
        inner: ShardedRoom<G, P>,
        vision: V,
        lent_pos: fn(&Wire<G>) -> Option<V::Pos>,
    ) -> Self {
        Self {
            inner,
            vision,
            lent_pos,
            budget: Budget::new(DEFAULT_TEAM_BUDGET),
            over_budget: 0,
            delta: false,
            contents: Vec::new(),
            ledgers: Vec::new(),
            baselines: Baselines::default(),
            bodies: HashMap::new(),
            step: 0,
            tick: 0,
            encoded: 0,
        }
    }

    /// Ship delta snapshots (the team room's delta mode —
    /// [`crate::team::TeamRoom::with_delta`]); full frames otherwise.
    #[must_use]
    pub fn with_delta(mut self) -> Self {
        self.delta = true;
        self
    }

    /// Export at most `records` records per team per tick (members
    /// first); the default is [`DEFAULT_TEAM_BUDGET`].
    #[must_use]
    pub fn with_team_budget(mut self, records: usize) -> Self {
        self.budget.records = records;
        self
    }

    /// Let the game pick what an over-budget export keeps (A29): `rank`
    /// maps a record's wire value to its rank — the higher kept first —
    /// WITHIN the members and within what they see (members still come
    /// first); equal ranks fall back to the smaller wire id. The wire
    /// value is what the kit holds of every record at export time (a
    /// lent record has nothing else). Without a rank (the default) a cut
    /// keeps each tier's first records in the kit's order: own by wire
    /// id, then the border strip. Called only for a team whose set is
    /// over the budget, once per record of it.
    #[must_use]
    pub fn with_export_rank(mut self, rank: fn(&Wire<G>) -> u32) -> Self {
        self.budget.rank = Some(rank);
        self
    }

    /// Opt the wrapped shard in to crystallization (see
    /// [`ShardedRoom::with_crystallize`]).
    #[must_use]
    pub fn with_crystallize(mut self, policy: Crystallize) -> Self {
        self.inner = self.inner.with_crystallize(policy);
        self
    }

    /// Set the disconnect-park grace on the wrapped shard (see
    /// [`ShardedRoom::with_disconnect_grace`]).
    #[must_use]
    pub fn with_disconnect_grace(mut self, grace: std::time::Duration) -> Self {
        self.inner = self.inner.with_disconnect_grace(grace);
        self
    }

    /// Set the whole disconnect-park policy on the wrapped shard (see
    /// [`ShardedRoom::with_disconnect_policy`]).
    #[must_use]
    pub fn with_disconnect_policy(
        mut self,
        grace: Option<std::time::Duration>,
        to: gsb_core::room::ExpireTo,
    ) -> Self {
        self.inner = self.inner.with_disconnect_policy(grace, to);
        self
    }

    /// Override the disconnect policy for one cause on the wrapped shard
    /// (see [`crate::room::OpenRoom::with_disconnect_policy_for`];
    /// BACKLOG F27).
    #[must_use]
    pub fn with_disconnect_policy_for(
        mut self,
        cause: gsb_core::room::DisconnectCause,
        grace: Option<std::time::Duration>,
        to: gsb_core::room::ExpireTo,
    ) -> Self {
        self.inner = self.inner.with_disconnect_policy_for(cause, grace, to);
        self
    }

    /// The game this shard runs.
    pub fn game(&self) -> &G {
        self.inner.game()
    }

    /// The game this shard runs, for configuration after construction.
    pub fn game_mut(&mut self) -> &mut G {
        self.inner.game_mut()
    }

    /// Records the per-team budget cut from this shard's exports so far.
    /// Each export also reports its own cut to the core
    /// (`TeamExport::over_budget`), which counts it into the metrics
    /// sample (`RoomSample::team_over_budget`, Prometheus
    /// `gsb_room_team_over_budget_total`).
    pub fn over_budget(&self) -> u64 {
        self.over_budget
    }

    /// The team of `entity` (its [`TeamMember`]), if it has one.
    pub(in crate::sharded) fn team_of_entity(
        world: &bevy_ecs::prelude::World,
        entity: bevy_ecs::prelude::Entity,
    ) -> Option<Team> {
        world.get::<TeamMember>(entity).map(|m| m.0)
    }
}
