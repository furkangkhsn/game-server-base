//! The base delay ring (module `base`, round 5): a standing queue stays a
//! queue for as long as the ring remembers the floor before it; a route
//! that got longer becomes the base once the old slots age out; a silent
//! spell ages them all at once.

use super::*;
use crate::udp::congestion::base::{BASE_SLOT, BASE_SLOTS, BaseDelay};

/// Samples `rtt` ms every `every` ms for `secs` seconds from `t`; the
/// base after the last.
fn hold(b: &mut BaseDelay, t: &mut Instant, rtt: u64, every: u64, secs: u64) -> Duration {
    let mut base = None;
    for _ in 0..secs * 1000 / every {
        *t += ms(every);
        base = Some(b.sample(ms(rtt), *t));
    }
    base.expect("sampled")
}

/// A queue stands at 600 ms over a 20 ms floor: the base stays 20 ms for
/// at least (BASE_SLOTS − 1) × BASE_SLOT — 40 s, four times the window —
/// and the queue is the base at most BASE_SLOTS × BASE_SLOT on (a route
/// that got 600 ms longer, learned by forgetting).
#[test]
fn a_standing_queue_stays_a_queue_until_the_ring_forgets() {
    let mut t = Instant::now();
    let mut b = BaseDelay::new(t);
    assert_eq!(b.base(), None);
    assert_eq!(hold(&mut b, &mut t, 20, 250, 5), ms(20));
    let remembered = BASE_SLOT.as_secs() * (BASE_SLOTS as u64 - 1);
    assert_eq!(hold(&mut b, &mut t, 620, 250, remembered), ms(20));
    assert_eq!(remembered, 40);
    let forgotten = BASE_SLOT.as_secs() * BASE_SLOTS as u64 - remembered;
    assert_eq!(hold(&mut b, &mut t, 620, 250, forgotten + 5), ms(620));
}

/// The smallest slot is the base, whichever slot holds it; a silent
/// spell of a whole ring forgets every slot at once.
#[test]
fn the_base_is_the_smallest_slot_and_silence_ages_them_all() {
    let mut t = Instant::now();
    let mut b = BaseDelay::new(t);
    let slot = BASE_SLOT.as_secs();
    hold(&mut b, &mut t, 50, 1000, slot);
    hold(&mut b, &mut t, 30, 1000, slot);
    assert_eq!(
        hold(&mut b, &mut t, 40, 1000, slot / 2),
        ms(30),
        "the middle slot"
    );
    t += BASE_SLOT * BASE_SLOTS as u32;
    assert_eq!(b.sample(ms(70), t), ms(70), "a silent ring: all forgotten");
    assert_eq!(b.sample(ms(90), t + ms(10)), ms(70));
}
