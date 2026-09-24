//! The shard's half of the detach-hold veto and its ceiling — the room
//! actor's cases (`room::tests::hold`) on the shard path, plus what only
//! a shard has: a parked row CROSSING a seam keeps its ceiling (it is
//! measured from the detach, so a crossing must not restart it).

use super::*;
use crate::room::{Detach, ExpireTo};

mod rig;
use rig::*;

const GRACE: Duration = Duration::from_secs(20);
const CEILING: Duration = Duration::from_secs(600);
const SEC: Duration = Duration::from_secs(1);

fn timed(to: ExpireTo) -> Detach {
    Detach::Hold {
        grace: Some(GRACE),
        to,
    }
}

fn lone(knobs: &Knobs, decision: Detach, ceiling: Option<Duration>) -> ShardActor<(), (), (), ()> {
    let (to_other, _rx) = channel::<Msg>(4);
    shard(0, knobs, decision, ceiling, to_other)
}

#[test]
fn sharded_veto_extends_a_timed_hold_until_it_lifts() {
    let knobs = Knobs::default();
    let mut a = lone(&knobs, timed(ExpireTo::Despawn), Some(CEILING));
    let p = join_and_detach(&mut a, 1);
    knobs.veto(true);
    step(&mut a, 2);
    assert_eq!(knobs.asks(), 0, "not asked while the grace runs");
    age(&mut a, p, GRACE);
    for t in 3..8 {
        step(&mut a, t);
    }
    assert!(held(&a, p), "the veto holds it past its deadline");
    assert_eq!(knobs.asks(), 5, "asked once per sweep");
    knobs.veto(false);
    step(&mut a, 8);
    assert!(!a.conns.contains_key(&p), "released on the next ask");
    assert_eq!(counts(&a), (1, 0, 0));
}

#[test]
fn sharded_standing_veto_is_overridden_at_the_ceiling_once_warned() {
    let knobs = Knobs::default();
    let untimed = Detach::Hold {
        grace: None,
        to: ExpireTo::AiHandover,
    };
    let mut a = lone(&knobs, untimed, Some(CEILING));
    let (p, q) = (join_and_detach(&mut a, 1), join_and_detach(&mut a, 2));
    knobs.veto(true);
    for x in [p, q] {
        age(&mut a, x, CEILING - SEC);
    }
    step(&mut a, 2);
    assert!(held(&a, p) && held(&a, q), "short of the ceiling");
    for x in [p, q] {
        age(&mut a, x, SEC);
    }
    step(&mut a, 3);
    assert!(
        a.conns[&p].bot_fed && a.conns[&q].bot_fed,
        "forced toward their ExpireTo (the untimed hold is bounded too)"
    );
    assert_eq!(counts(&a), (0, 2, 1), "one warning for the shard");
}

#[test]
fn sharded_logic_that_never_vetoes_ends_holds_where_it_did() {
    let knobs = Knobs::default();
    let mut a = lone(&knobs, timed(ExpireTo::Despawn), Some(CEILING));
    let p = join_and_detach(&mut a, 1);
    age(&mut a, p, GRACE - SEC);
    step(&mut a, 2);
    assert!(held(&a, p), "not before the deadline");
    age(&mut a, p, SEC);
    step(&mut a, 3);
    assert!(!a.conns.contains_key(&p), "on the deadline's sweep");
    assert_eq!(counts(&a), (1, 0, 0));

    let long = Detach::Hold {
        grace: Some(CEILING * 2),
        to: ExpireTo::Despawn,
    };
    let mut a = lone(&knobs, long, Some(CEILING));
    let p = join_and_detach(&mut a, 1);
    age(&mut a, p, CEILING + SEC);
    step(&mut a, 2);
    assert!(held(&a, p), "the ceiling never shortens a grace");
}

#[test]
fn sharded_zero_and_absent_ceilings_read_literally() {
    let knobs = Knobs::default();
    knobs.veto(true);
    let mut a = lone(&knobs, timed(ExpireTo::Despawn), Some(Duration::ZERO));
    let p = join_and_detach(&mut a, 1);
    step(&mut a, 2);
    assert!(held(&a, p), "the grace itself still runs");
    age(&mut a, p, GRACE);
    step(&mut a, 3);
    assert_eq!(counts(&a), (1, 0, 1), "zero: forced at the deadline");

    let mut a = lone(&knobs, timed(ExpireTo::Despawn), None);
    let p = join_and_detach(&mut a, 1);
    assert_eq!(a.conns[&p].detach_ceiling, None);
    age(&mut a, p, GRACE * 1000);
    step(&mut a, 2);
    assert!(held(&a, p), "None: a standing veto holds");
}

/// A parked player's entity migrates like any other (§3.2); its ceiling
/// rides the crossing unchanged, and the receiving shard enforces it.
#[test]
fn a_parked_row_keeps_its_ceiling_across_a_crossing() {
    let knobs = Knobs::default();
    let (to_s1, mut s1_inbox) = channel::<Msg>(4);
    let mut s0 = shard(0, &knobs, timed(ExpireTo::Despawn), Some(CEILING), to_s1);
    let p = join_and_detach(&mut s0, 1);
    knobs.veto(true);
    age(&mut s0, p, CEILING - SEC);
    let ceiling = s0.conns[&p].detach_ceiling;

    knobs.evict(p);
    step(&mut s0, 2);
    assert!(!s0.conns.contains_key(&p), "the row left with its entity");
    let msg = s1_inbox.try_recv().expect("the crossing was sent");
    let ShardMsg::Migrate {
        at_tick,
        player: Some(pm),
        ..
    } = &msg
    else {
        panic!("expected a player migration, got {msg:?}");
    };
    assert_eq!(pm.detach_ceiling, ceiling, "the ceiling travels as is");
    let at_tick = *at_tick;

    let (to_s0, _s0_inbox) = channel::<Msg>(4);
    let mut s1 = shard(1, &knobs, timed(ExpireTo::Despawn), Some(CEILING), to_s0);
    assert!(s1.handle_msg(msg, at_tick + 1));
    step(&mut s1, at_tick + 1);
    assert!(held(&s1, p), "still short of the ceiling after the move");
    age(&mut s1, p, SEC);
    step(&mut s1, at_tick + 2);
    assert!(!s1.conns.contains_key(&p), "the new owner forced it out");
    assert_eq!(counts(&s1), (1, 0, 1));
}
