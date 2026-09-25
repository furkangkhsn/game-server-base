//! [`MmoGame`] — the MMO's kit hooks (KIT-ARCHITECTURE §4.3/§4.6):
//! [`Game`] (spawn a character at its saved position, `MoveTo` /
//! `Attack` / `Travel` input, the camps + movement + lifecycle systems,
//! the optional logout bot) and [`ShardGame`] (`MmoMig`, `capture`,
//! `restore`).
//!
//! One instance per SHARD: a shard's game spawns mobs only from the
//! camps on its own ground ([`Realm::spawns_of`]); every shard knows the
//! saved characters, keyed by the player's authenticated identity (a
//! login is routed to the shard owning its saved position —
//! [`crate::world::home_shard`] — by the same identity:
//! `docs/GAME-MODULE.md`, K4).
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
use std::sync::Arc;

use bevy_ecs::prelude::{Entity, World};
use gsb_core::channel::Mailbox;
use gsb_core::id::{ConnectionId, PlayerId};
use gsb_core::room::{Action, TickCtx};
use gsb_core::shard::{EffectOutcome, RemoteEffect};
use gsb_kit::game::{Game, InputSeq, ShardGame};
use gsb_kit::sharded::Seam;
use prost::Message;

use crate::codec::{MmoCodec, MmoWire, to_dm};
use crate::combat::{Combat, Hit};
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
    /// Saved character positions (the realm's shared table), by the
    /// player's authenticated identity.
    logins: Arc<HashMap<String, Pos3>>,
    camps: Camps,
    systems: Systems,
    combat: Combat,
    codec: MmoCodec,
}

impl MmoGame {
    /// The game of shard `index`, from the realm's data.
    #[must_use]
    pub fn for_shard(index: usize, realm: &Realm) -> Self {
        Self {
            index,
            logins: Arc::clone(&realm.logins),
            camps: Camps::new(realm.spawns_of(index).cloned()),
            systems: Systems::default(),
            combat: Combat {
                shard: index,
                feed: None,
            },
            codec: MmoCodec,
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

impl Game for MmoGame {
    type Codec = MmoCodec;

    const SNAPSHOT_OP: u16 = op::MMO_SNAPSHOT;
    const PRIVATE_OP: u16 = op::MMO_PRIVATE;

    fn codec(&self) -> &MmoCodec {
        &self.codec
    }

    /// An anonymous session has no saved character: it spawns at this
    /// shard's waystone.
    fn spawn_player(&mut self, world: &mut World, conn: ConnectionId) -> Entity {
        self.spawn_player_as(world, conn, "")
    }

    /// Spawn the character of the player who logged in as `identity` at
    /// its saved position — or, with no saved character, at this
    /// shard's waystone.
    fn spawn_player_as(
        &mut self,
        world: &mut World,
        _conn: ConnectionId,
        identity: &str,
    ) -> Entity {
        let at = self.logins.get(identity).copied().unwrap_or_else(|| {
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
        input::ingest(players, world, actions, seq, ctx.tick, &self.combat, None);
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

    /// The sharded input path: an `Attack` on an entity a neighbour
    /// lends becomes a remote effect for its owner.
    fn ingest_seam(
        &mut self,
        world: &mut World,
        ctx: &TickCtx,
        actions: &mut Vec<Action>,
        players: &HashMap<PlayerId, Entity>,
        seq: &mut InputSeq,
        seam: &mut Seam<'_, '_, MmoWire>,
    ) {
        let combat = &self.combat;
        input::ingest(players, world, actions, seq, ctx.tick, combat, Some(seam));
    }

    /// A neighbour's attack on one of this shard's entities.
    fn apply_remote_effect(
        &mut self,
        world: &mut World,
        target: Entity,
        effect: &RemoteEffect,
        tick: u64,
        seam: &mut Seam<'_, '_, MmoWire>,
    ) -> EffectOutcome {
        self.combat.apply_remote(world, target, effect, tick, seam)
    }
}

#[cfg(test)]
mod tests;
