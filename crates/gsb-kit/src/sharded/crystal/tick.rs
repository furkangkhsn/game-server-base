//! The once-per-tick pass (after the systems, before the migrate phase
//! reads the pins): end the holds that are over, then pin the movers of
//! the fights that are ripe.

use std::collections::HashMap;

use bevy_ecs::prelude::{Entity, World};
use gsb_core::shard::CrossSeam;

use crate::sharded::crystal::{Crystal, Pin, Release};
use crate::space::Partition;

impl Crystal {
    /// Run the pass for shard `index` at `tick`. `own` is the room's
    /// wire → entity table (accurate: the tick's despawns are swept),
    /// `cross` the tick's lent view (who lends a partner = where a mover
    /// goes).
    pub(in crate::sharded) fn evaluate<W, P: Partition<W>>(
        &mut self,
        world: &World,
        tick: u64,
        index: usize,
        partition: &P,
        cross: &CrossSeam<'_, W>,
        own: &HashMap<u64, Entity>,
    ) {
        let policy = self.policy;
        self.book.expire(tick, policy.window);
        let pos_of = |wire: u64| own.get(&wire).and_then(|&e| world.get::<P::Pos>(e));

        // Holds first: a hold that ends this tick frees its entity to be
        // a mover again only from the next tick on (it is not in the
        // table's ripe set as a mover while pinned — below).
        if !self.pins.is_empty() {
            self.pins.retain(|&wire, pin| {
                let Some(pos) = pos_of(wire) else {
                    return false; // gone (despawned, left)
                };
                let quiet = tick.saturating_sub(pin.last) > policy.release;
                if pin.anchor != index {
                    // A mover whose migration has not gone out yet (a full
                    // neighbour inbox): it keeps trying while the fight
                    // lasts.
                    return !quiet;
                }
                let why = if !own.contains_key(&pin.partner) {
                    Some(Release::Partner)
                } else if quiet {
                    Some(Release::Quiet)
                } else if !partition.holds(index, pos, policy.margin) {
                    Some(Release::Band)
                } else {
                    None
                };
                if let Some(why) = why {
                    tracing::debug!(
                        target: "gsb_kit::crystal",
                        shard = index,
                        wire,
                        partner = pin.partner,
                        ?why,
                        "crystal_release"
                    );
                }
                why.is_none()
            });
        }

        // Movers: the higher wire of a ripe pair, owned here and not
        // held, whose partner a neighbour lends and which stands inside
        // the partner shard's entering band (half the margin — the
        // spatial hysteresis). Sorted, so a mover with several ripe
        // partners follows the LOWEST one: every fight converges on the
        // shard of its lowest wire, which never moves for it.
        let mut ripe: Vec<(u64, u64)> = self
            .book
            .ripe(tick, policy.after, policy.window)
            .filter(|(lo, hi)| own.contains_key(hi) && !own.contains_key(lo))
            .collect();
        if ripe.is_empty() {
            return;
        }
        ripe.sort_unstable();
        for (lo, hi) in ripe {
            if self.pins.contains_key(&hi) {
                continue;
            }
            let (Some(lent), Some(pos)) = (cross.lent(lo), pos_of(hi)) else {
                continue;
            };
            let anchor = lent.lender;
            if !partition.holds(anchor, pos, policy.margin / 2.0) {
                continue;
            }
            self.pins.insert(
                hi,
                Pin {
                    anchor,
                    partner: lo,
                    last: tick,
                },
            );
            tracing::debug!(
                target: "gsb_kit::crystal",
                shard = index,
                wire = hi,
                partner = lo,
                anchor,
                "crystal_move"
            );
        }
    }
}
