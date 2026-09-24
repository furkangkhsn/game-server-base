//! [`MmoGame`] — the MMO's kit hooks (KIT-ARCHITECTURE §4.3/§4.6):
//! [`Game`] (spawn a character at its saved position, `MoveTo` /
//! `Attack` / `Travel` input, the camps + movement + lifecycle systems,
//! the optional logout bot) and [`ShardGame`] (`MmoMig`, `capture`,
//! `restore`).
//!
//! One instance per SHARD: a shard's game spawns mobs only from the
//! camps on its own ground ([`Realm::spawns_of`]); every shard knows the
//! saved characters (a login is routed to the shard owning its saved
//! position — [`crate::world::home_shard`]).
//!
//! **Disconnects (the kit's park machinery).** A dropped session's
//! character is PARKED for the room's grace (it stays in the world, keeps
//! its wire id and slot; a resume within the grace reclaims it); when the
//! grace runs out the logout completes — unless the character is IN
//! COMBAT ([`InCombat`], set by its landed attacks): then
//! [`Game::may_release`] vetoes and the logout waits for the fight to
//! end (bounded by the core's `RoomConfig::max_detach_hold`,
//! `docs/RECONNECT.md` §17). The kit then releases the slot and the
//! character leaves the world — the MMO's usual logout timer
//! ([`crate::mmo_shard`]'s policy; before the kit's Phase 5 every hold
//! ended in AI handover — `docs/KIT-ARCHITECTURE.md` §10, F4). A room
//! built to end the hold in AI handover instead gets the logout bot:
//! [`Game::bot_actions`] walks the character to the nearest waystone, a
//! safe spot, where it stays.

use std::collections::HashMap;

use bevy_ecs::prelude::{Entity, World};
use gsb_core::id::{ConnectionId, PlayerId};
use gsb_core::room::{Action, TickCtx};
use gsb_kit::game::{Game, InputSeq, ShardGame};
use prost::Message;

use crate::codec::{MmoCodec, to_dm};
use crate::components::{InCombat, Kind, MoveTarget, Pos3, RunSpeed, Vitals};
use crate::migrate::{self, MmoMig};
use crate::realm::Realm;
use crate::systems::{Camps, Systems};
use crate::world::{PLAYER_HP, RUN_SPEED, WAYSTONES, nearest_waystone};
use crate::{input, op};

/// The MMO game of one shard.
pub struct MmoGame {
    /// This shard's region index.
    index: usize,
    /// Saved character positions (the realm's), by login session.
    logins: HashMap<ConnectionId, Pos3>,
    camps: Camps,
    systems: Systems,
    codec: MmoCodec,
}

impl MmoGame {
    /// The game of shard `index`, from the realm's data.
    #[must_use]
    pub fn for_shard(index: usize, realm: &Realm) -> Self {
        Self {
            index,
            logins: realm.logins.clone(),
            camps: Camps::new(realm.spawns_of(index).cloned()),
            systems: Systems::default(),
            codec: MmoCodec,
        }
    }

    /// This shard's region index.
    pub fn index(&self) -> usize {
        self.index
    }
}

impl Game for MmoGame {
    type Codec = MmoCodec;

    const SNAPSHOT_OP: u16 = op::MMO_SNAPSHOT;
    const PRIVATE_OP: u16 = op::MMO_PRIVATE;

    fn codec(&self) -> &MmoCodec {
        &self.codec
    }

    /// Spawn the character at its saved position — or, for a session
    /// with no saved character, at this shard's waystone.
    fn spawn_player(&mut self, world: &mut World, conn: ConnectionId) -> Entity {
        let at = self.logins.get(&conn).copied().unwrap_or_else(|| {
            let [x, z] = WAYSTONES[self.index % WAYSTONES.len()];
            Pos3::new(x, 0.0, z)
        });
        let vitals = Vitals {
            kind: Kind::Player,
            hp: PLAYER_HP,
        };
        world
            .spawn((at.clamped(), vitals, RunSpeed(RUN_SPEED)))
            .id()
    }

    /// The logout bot (a room whose disconnect policy ends in AI handover
    /// — not the MMO's default): a character whose disconnect grace ran out walks
    /// to the nearest waystone through the ordinary input path (an
    /// unnumbered `MoveTo`, decoded by [`Game::ingest`] like a client's).
    /// Sent only while it is not already walking there.
    fn bot_actions(
        &mut self,
        world: &World,
        _ctx: &TickCtx,
        bots: impl Iterator<Item = (PlayerId, Entity)>,
        out: &mut Vec<Action>,
    ) {
        for (player, entity) in bots {
            let Ok(e) = world.get_entity(entity) else {
                continue;
            };
            let Some(pos) = e.get::<Pos3>() else {
                continue;
            };
            let [x, z] = nearest_waystone(pos);
            if e.get::<MoveTarget>() == Some(&MoveTarget { x, z }) {
                continue; // already walking there
            }
            let msg = crate::mmo::MoveTo {
                x: to_dm(x),
                z: to_dm(z),
                seq: 0,
            };
            out.push(Action {
                conn: ConnectionId(0), // no transport behind a bot
                player,
                op: op::MMO_MOVE_TO,
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
        input::ingest(players, world, actions, seq, ctx.tick);
    }

    fn systems(&mut self, world: &mut World, ctx: &TickCtx) {
        self.camps.run(world, ctx.tick);
        self.systems.run(world, ctx.tick, ctx.dt.as_secs_f32());
    }

    /// No logout in combat: a parked character whose disconnect grace
    /// ran out stays in the world while it is [`InCombat`]; the core asks
    /// again every tick, and the logout completes on the first tick after
    /// the fight has cooled down.
    fn may_release(&mut self, world: &mut World, entity: Entity) -> bool {
        !world
            .get_entity(entity)
            .is_ok_and(|e| e.contains::<InCombat>())
    }
}

impl ShardGame for MmoGame {
    type Mig = MmoMig;

    fn capture(&self, world: &World, entity: Entity) -> MmoMig {
        migrate::capture(world, entity)
    }

    fn restore(&mut self, world: &mut World, mig: MmoMig) -> Entity {
        migrate::restore(world, mig)
    }
}

#[cfg(test)]
mod tests;
