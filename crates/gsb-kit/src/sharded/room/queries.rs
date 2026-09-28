//! A shard's world queries, kept across ticks (`crate::common::Cached`,
//! KIT-ARCHITECTURE §10 "A12"): built once per shard, only iterated on
//! the tick — rebuilt only when the world grew a new archetype.

use bevy_ecs::prelude::{Entity, With};

use crate::codec::RecordCodec;
use crate::common::{Cached, Orphans, RecordPass};
use crate::game::{Game, ShardGame, Wire};
use crate::identity::WireId;
use crate::space::Partition;

/// The game's broadcast marker (the codec's `Marker`).
type Marker<G> = <<G as Game>::Codec as RecordCodec>::Marker;
/// The game's record query (the codec's `Query`).
type RecordQuery<G> = <<G as Game>::Codec as RecordCodec>::Query;
/// The partition's position component.
type Pos<G, P> = <P as Partition<Wire<G>>>::Pos;

/// The border export's query: every broadcastable entity's wire
/// identity, record components and position.
type Border<G, P> = Cached<(&'static WireId, RecordQuery<G>, &'static Pos<G, P>), With<Marker<G>>>;

/// The migration scan's query: the border export's, with the entity.
type Crossing<G, P> =
    Cached<(Entity, &'static WireId, &'static Pos<G, P>, RecordQuery<G>), With<Marker<G>>>;

/// A shard's kept world queries (see `ShardedRoom::queries`).
pub(in crate::sharded) struct Queries<G: ShardGame, P: Partition<Wire<G>>> {
    /// The orphan stamp (`ShardedRoom::step`).
    pub(in crate::sharded) orphans: Orphans<Marker<G>>,
    /// The border-cache rebuild (`ShardedRoom::step`).
    pub(in crate::sharded) border: Border<G, P>,
    /// The own records (`ShardedRoom::own_records`).
    pub(in crate::sharded) own: RecordPass<G::Codec>,
    /// The migration scan (`collect_migrations`).
    pub(in crate::sharded) crossing: Crossing<G, P>,
}

impl<G: ShardGame, P: Partition<Wire<G>>> Default for Queries<G, P> {
    fn default() -> Self {
        Self {
            orphans: Cached::default(),
            border: Cached::default(),
            own: Cached::default(),
            crossing: Cached::default(),
        }
    }
}
