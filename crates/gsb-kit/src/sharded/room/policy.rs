//! The room's policy builders: what the factory sets on every shard
//! of a room alike — crystallization and the disconnect-park policy.

use crate::game::{ShardGame, Wire};
use crate::sharded::Crystallize;
use crate::sharded::crystal::Crystal;
use crate::sharded::room::ShardedRoom;
use crate::space::Partition;

impl<G: ShardGame, P: Partition<Wire<G>>> ShardedRoom<G, P> {
    /// Opt in to crystallization (`docs/CROSS-SHARD.md` §4 layer 4): a
    /// fight that keeps going across a seam moves onto one shard — its
    /// higher wire id migrates to the shard of the lower one and is held
    /// there, with its partner, until the fight is over (see
    /// [`Crystallize`]). Every shard of a room should carry the same
    /// policy. Without it the room moves an entity only by its region.
    #[must_use]
    pub fn with_crystallize(mut self, policy: Crystallize) -> Self {
        self.crystal = Some(Crystal::new(policy));
        self
    }

    /// Set the disconnect-park grace (see
    /// [`crate::room::OpenRoom::with_disconnect_grace`]; RECONNECT §3).
    /// Every shard of a room should carry the same policy (the factory
    /// builds them uniformly).
    #[must_use]
    pub fn with_disconnect_grace(mut self, grace: std::time::Duration) -> Self {
        self.park.grace = Some(grace);
        self
    }

    /// Set the whole disconnect-park policy (see
    /// [`crate::room::OpenRoom::with_disconnect_policy`]; RECONNECT
    /// §3/§14.4).
    #[must_use]
    pub fn with_disconnect_policy(
        mut self,
        grace: Option<std::time::Duration>,
        to: gsb_core::room::ExpireTo,
    ) -> Self {
        self.park.grace = grace;
        self.park.to = to;
        self
    }

    /// Override the disconnect policy for one cause (see
    /// [`crate::room::OpenRoom::with_disconnect_policy_for`]; BACKLOG
    /// F27). Every shard of a room should carry the same overrides.
    #[must_use]
    pub fn with_disconnect_policy_for(
        mut self,
        cause: gsb_core::room::DisconnectCause,
        grace: Option<std::time::Duration>,
        to: gsb_core::room::ExpireTo,
    ) -> Self {
        self.park.set_for(cause, grace, to);
        self
    }
}
