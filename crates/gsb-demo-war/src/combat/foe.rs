//! [`Foe`]: a combatant as the war's attack reads it, the same fields
//! from one of this shard's units or from a neighbour's lent record —
//! the kit's `SeamView`, through which `Seam::find` answers a wire id
//! with the seam's precedence (KIT-ARCHITECTURE §10 "F6").

use bevy_ecs::prelude::{Entity, World};
use bevy_ecs::world::EntityRef;
use gsb_kit::identity::WireId;
use gsb_kit::sharded::{Found, Holder, SeamView};
use gsb_kit::team::Team;

use super::standing;
use crate::codec::{WarWire, team_of_wire};
use crate::components::{Kind, Pos3, Unit};

/// A combatant as an attack reads it — one of this shard's units or a
/// neighbour's lent record, the same fields either way (the kit's
/// [`SeamView`]: `Seam::find` picks which one answers a wire id).
#[derive(Debug, Clone, Copy)]
pub(super) struct Foe {
    pub(super) at: Pos3,
    pub(super) side: Option<Team>,
    pub(super) standing: bool,
}

impl SeamView<WarWire> for Foe {
    fn local(unit: EntityRef<'_>) -> Option<Self> {
        let (&at, u) = (unit.get::<Pos3>()?, unit.get::<Unit>()?);
        let (side, standing) = (u.faction, standing(u));
        Some(Foe { at, side, standing })
    }

    fn lent(w: &WarWire) -> Option<Self> {
        Some(Foe {
            at: w.pos(),
            side: team_of_wire(w.faction.into()),
            standing: w.kind == Kind::Player && w.hp > 0,
        })
    }
}

/// With no seam (a single-world room): the unit with wire id `wire` in
/// this world.
pub(super) fn local_foe(world: &mut World, wire: u64) -> Option<Found<Foe>> {
    let mut q = world.query::<(Entity, &WireId)>();
    let (victim, _) = q.iter(world).find(|(_, w)| w.get() == wire)?;
    let view = Foe::local(world.entity(victim))?;
    let holder = Holder::Local(victim);
    Some(Found { wire, holder, view })
}
