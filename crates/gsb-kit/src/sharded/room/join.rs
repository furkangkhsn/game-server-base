//! The join's admission, shared by the room and the team composite: the
//! two identities from this shard's range, the tables that learn them.

use bevy_ecs::prelude::{Entity, World};
use gsb_core::room::Admission;

use crate::game::{ShardGame, Wire};
use crate::sharded::room::*;
use crate::space::Partition;

impl<G: ShardGame, P: Partition<Wire<G>>> ShardedRoom<G, P> {
    /// Admit a joining player whose entity `spawn` creates: the stable
    /// player identity and the wire identity both come from this shard's
    /// range-partitioned counter (player first, as it always was), and
    /// the tables learn both. Shared by this room's join and the team
    /// composite's (which spawns through `TeamGame`).
    pub(in crate::sharded) fn admit(
        &mut self,
        world: &mut World,
        spawn: impl FnOnce(&mut G, &mut World) -> Entity,
    ) -> Admission {
        let player = self.mint_player();
        let entity = spawn(&mut self.game, world);
        debug_assert!(
            world.entity(entity).contains::<Marker<G>>(),
            "Game::spawn_player must spawn the codec's Marker (the broadcast set)"
        );
        let wire_id = self.minter.mint();
        let wire = wire_id.get();
        world.entity_mut(entity).insert(wire_id);
        self.player_entity.insert(player, entity);
        self.entity_player.insert(entity, player);
        self.wire_entity.insert(wire, entity);
        self.entity_wire.insert(entity, wire);
        self.input.begin(player);
        Admission {
            player,
            entity: wire,
        }
    }
}
