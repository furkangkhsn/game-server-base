//! The duelists (`--mmo-duel-frac F`, default 0: none): a fraction of the
//! MMO's bots that fight ACROSS a shard seam — the load the default bot,
//! roaming 256 m from every seam, never makes (the cross-seam effects of
//! `docs/CROSS-SHARD.md` §2–§4 and their crystallization).
//!
//! **Who.** Bots come in id pairs `(2j, 2j + 1)`; pair `j` duels when
//! `frac(j · φ⁻¹) < F` (the golden-ratio sequence: the realized fraction
//! is within one pair of `F` for any client count, and the choice is a
//! pure function of the id — a partitioned run picks the same bots). No
//! draw of the bot's own stream is spent on it, so with `F = 0` every
//! bot's inputs are exactly the default bot's.
//!
//! **Where.** The even bot of a pair stands 8 m west of the x = 0 seam,
//! the odd one 8 m east, on one of sixteen spots 32 m apart (eight
//! round z = −256 m, eight round z = +256 m — the rows of the waystones,
//! so the walk from a waystone is the shortest a seam allows, ~250 m;
//! 32 m is beyond the 30 m reach, so a spot's fight stays its own). A
//! duelist first `Travel`s to the waystone on its side of the seam and
//! walks to its spot.
//!
//! **What.** At the spot it `Attack`s the nearest player ACROSS the seam
//! within reach at the bot's attack rate (one per second on average) —
//! its partner, or anyone else at the spot; otherwise it walks (back) to
//! its spot. A defeated duelist wakes at its waystone and walks back: the
//! fights recur for the whole run.

use gsb_demo_mmo::mmo::Kind;
use gsb_demo_mmo::world::ATTACK_RANGE;

use super::MmoRecord;

/// The spots per row (two rows).
const SPOTS_PER_ROW: u64 = 8;
/// The distance between two spots along the seam (metres).
const SPOT_STEP: f32 = 32.0;
/// A duelist's distance from the seam (metres).
const SPOT_OFF: f32 = 8.0;

/// A duelist's post: its spot on the ground (metres) and the waystone on
/// its side of the seam.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Duel {
    pub(crate) x: f32,
    pub(crate) z: f32,
    pub(crate) waystone: usize,
}

/// Client `id`'s post, when its pair duels at fraction `frac`.
pub(crate) fn duel_of(id: u64, frac: f64) -> Option<Duel> {
    let pair = id / 2;
    let phase = (pair as f64 * 0.618_033_988_749_894_9).fract();
    if frac <= 0.0 || phase >= frac {
        return None;
    }
    let west = id.is_multiple_of(2);
    let spot = pair % (2 * SPOTS_PER_ROW);
    let north = spot >= SPOTS_PER_ROW;
    let along = (spot % SPOTS_PER_ROW) as f32 - (SPOTS_PER_ROW as f32 - 1.0) / 2.0;
    let row = if north { 256.0 } else { -256.0 };
    Some(Duel {
        x: if west { -SPOT_OFF } else { SPOT_OFF },
        z: row + along * SPOT_STEP,
        // Waystones 0..3 are the (west, east) × (south, north) regions.
        waystone: usize::from(!west) + 2 * usize::from(north),
    })
}

/// The nearest player across the x = 0 seam from `me` (decimetres),
/// within the game's reach (3D), among `view`.
pub(crate) fn foe_in_reach<'a>(
    me: &MmoRecord,
    view: impl Iterator<Item = (u64, &'a MmoRecord)>,
) -> Option<u64> {
    let reach = f64::from(ATTACK_RANGE) * 10.0;
    view.filter(|(_, r)| r.kind == Kind::Player as i32 && (r.x < 0) != (me.x < 0))
        .map(|(id, r)| (id, me.dist_dm(r)))
        .filter(|&(_, d)| d <= reach)
        .min_by(|a, b| a.1.total_cmp(&b.1))
        .map(|(id, _)| id)
}

#[cfg(test)]
mod tests;
