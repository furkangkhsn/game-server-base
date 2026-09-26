//! [`WarGame`] — the war game's kit hooks (KIT-ARCHITECTURE §4.3/§4.6):
//! [`Game`] (`MoveTo` / `Attack` input, the systems, the retreat bot,
//! the session's `Welcome`), [`TeamGame`] (the faction and the spawn,
//! decided together from the authenticated identity) and [`ShardGame`]
//! (`WarMig`, cross-seam attacks as remote effects).
//!
//! One instance per SHARD. Every shard knows the saved characters
//! ([`Realm`]); a login is routed to the shard owning its placement
//! (`world::home_shard` of `Realm::placement` — the server module's
//! router reads the same table by the same identity, K4).
//!
//! **Disconnects** use the kit's default park policy: the unit stays
//! (parked, resumable) for the room's grace, then the kit hands it to
//! the retreat bot ([`Game::bot_actions`]), which walks it home to its
//! faction's base — the arena's choice: a war has no logout timer.

use std::collections::HashMap;

use bevy_ecs::prelude::{Entity, World};
use bytes::BytesMut;
use gsb_core::channel::Mailbox;
use gsb_core::id::{ConnectionId, PlayerId};
use gsb_core::metrics::LogicCounters;
use gsb_core::room::{Action, TickCtx};
use gsb_core::shard::{EffectOutcome, RemoteEffect};
use gsb_kit::game::{Game, InputSeq, ShardGame, TeamGame};
use gsb_kit::sharded::Seam;
use gsb_kit::team::{Team, TeamMember};
use prost::Message;

use crate::codec::{WarCodec, WarWire, to_dm, wire_faction};
use crate::combat::{Combat, Hit};
use crate::components::{Kind, MoveTarget, Unit};
use crate::migrate::{self, WarMig};
use crate::realm::Realm;
use crate::systems::Systems;
use crate::world::{FACTIONS, PLAYER_HP, base};
use crate::{input, op};

/// The war game of one shard.
pub struct WarGame {
    /// This shard's region index.
    index: usize,
    realm: Realm,
    systems: Systems,
    combat: Combat,
    codec: WarCodec,
}

impl WarGame {
    /// The game of shard `index`, from the realm's data.
    #[must_use]
    pub fn for_shard(index: usize, realm: &Realm) -> Self {
        Self {
            index,
            realm: realm.clone(),
            systems: Systems::new(index),
            combat: Combat {
                shard: index,
                feed: None,
                kills: 0,
            },
            codec: WarCodec,
        }
    }

    /// This shard's region index.
    pub fn index(&self) -> usize {
        self.index
    }

    /// Publish every hit this shard applies on `feed` (the kill feed —
    /// `try_send`, a full feed drops; it is observability, not play).
    pub fn set_combat_feed(&mut self, feed: Mailbox<Hit>) {
        self.combat.feed = Some(feed);
    }
}

impl Game for WarGame {
    type Codec = WarCodec;

    const SNAPSHOT_OP: u16 = op::WAR_SNAPSHOT;
    const PRIVATE_OP: u16 = op::WAR_PRIVATE;

    fn codec(&self) -> &WarCodec {
        &self.codec
    }

    /// A unit for `conn` outside a team room (the kit's team rooms call
    /// [`TeamGame::spawn_team_player_as`]): the anonymous placement.
    fn spawn_player(&mut self, world: &mut World, conn: ConnectionId) -> Entity {
        self.spawn_team_player_as(world, conn, "").0
    }

    /// The retreat bot: a unit whose player's disconnect grace ran out
    /// walks home to its faction's base, through the ordinary input path
    /// (an unnumbered `MoveTo`, decoded by [`Game::ingest`] like a
    /// client's). Sent only while it is not already walking there.
    fn bot_actions(
        &mut self,
        world: &World,
        _ctx: &TickCtx,
        bots: impl Iterator<Item = (PlayerId, Entity)>,
        out: &mut Vec<Action>,
    ) {
        for (player, entity) in bots {
            let Some(&TeamMember(faction)) = world.get::<TeamMember>(entity) else {
                continue;
            };
            let home = base(faction);
            if world.get::<MoveTarget>(entity)
                == Some(&MoveTarget {
                    x: home.x,
                    z: home.z,
                })
            {
                continue; // already walking home
            }
            let msg = crate::war::MoveTo {
                x: to_dm(home.x),
                z: to_dm(home.z),
                seq: 0,
            };
            out.push(Action {
                conn: ConnectionId(0), // no transport behind a bot
                player,
                op: op::WAR_MOVE_TO,
                payload: msg.encode_to_vec().into(),
            });
        }
    }

    fn ingest(
        &mut self,
        world: &mut World,
        ctx: &TickCtx,
        actions: &mut Vec<Action>,
        players: &HashMap<PlayerId, Entity>,
        seq: &mut InputSeq,
    ) {
        input::ingest(
            players,
            world,
            actions,
            seq,
            ctx.tick,
            &mut self.combat,
            None,
        );
    }

    fn systems(&mut self, world: &mut World, ctx: &TickCtx) {
        self.systems.run(world, ctx.dt.as_secs_f32());
    }

    /// The session's `Welcome`: the unit's faction (1-based, as the
    /// kit recorded it) and the number of factions — what a client needs
    /// to tell allies from enemies and find its base.
    fn session_private(&mut self, world: &World, entity: Entity, out: &mut BytesMut) -> bool {
        let Some(&TeamMember(faction)) = world.get::<TeamMember>(entity) else {
            return false;
        };
        let welcome = crate::war::Welcome {
            faction: u32::from(wire_faction(Some(faction))),
            factions: u32::from(FACTIONS),
        };
        welcome
            .encode(out)
            .expect("protobuf encode into an in-memory buffer failed");
        true
    }

    /// The war's own counter: the kills this shard's combat applied
    /// ([`crate::combat::KILLS`]).
    fn counters(&self, _world: &World, out: &mut LogicCounters) {
        out.put(&crate::combat::KILLS, self.combat.kills);
    }
}

/// The faction and the spawn, decided together (module docs).
impl TeamGame for WarGame {
    /// The anonymous session: the empty identity's placement.
    fn spawn_team_player(&mut self, world: &mut World, conn: ConnectionId) -> (Entity, Team) {
        self.spawn_team_player_as(world, conn, "")
    }

    /// Spawn the unit of the player who logged in as `identity` where
    /// the realm places it (its saved character, or its hashed faction's
    /// base) and on that side; the kit writes the faction as the unit's
    /// `TeamMember`.
    fn spawn_team_player_as(
        &mut self,
        world: &mut World,
        _conn: ConnectionId,
        identity: &str,
    ) -> (Entity, Team) {
        let placed = self.realm.placement(identity);
        let unit = Unit {
            kind: Kind::Player,
            faction: Some(placed.faction),
            hp: PLAYER_HP,
        };
        (
            world.spawn((placed.at.clamped(), unit)).id(),
            placed.faction,
        )
    }

    /// The faction of `entity`, as the kit recorded it (not asked — the
    /// spawn decides it; the answer stays true for any caller).
    fn team_of(&mut self, world: &World, _conn: ConnectionId, entity: Entity) -> Team {
        world.get::<TeamMember>(entity).map_or(Team(0), |m| m.0)
    }
}

impl ShardGame for WarGame {
    type Mig = WarMig;

    fn capture(&self, world: &World, entity: Entity) -> WarMig {
        migrate::capture(world, entity)
    }

    fn restore(&mut self, world: &mut World, mig: WarMig) -> Entity {
        migrate::restore(world, mig)
    }

    /// The sharded input path: an `Attack` on an enemy a neighbour lends
    /// becomes a remote effect for its owner.
    fn ingest_seam(
        &mut self,
        world: &mut World,
        ctx: &TickCtx,
        actions: &mut Vec<Action>,
        players: &HashMap<PlayerId, Entity>,
        seq: &mut InputSeq,
        seam: &mut Seam<'_, '_, WarWire>,
    ) {
        let combat = &mut self.combat;
        input::ingest(players, world, actions, seq, ctx.tick, combat, Some(seam));
    }

    /// A neighbour's attack on one of this shard's players.
    fn apply_remote_effect(
        &mut self,
        world: &mut World,
        target: Entity,
        effect: &RemoteEffect,
        tick: u64,
        seam: &mut Seam<'_, '_, WarWire>,
    ) -> EffectOutcome {
        self.combat.apply_remote(world, target, effect, tick, seam)
    }
}

#[cfg(test)]
mod tests;
