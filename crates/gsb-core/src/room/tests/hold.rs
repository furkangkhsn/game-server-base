//! The detach-hold veto and its ceiling (`docs/RECONNECT.md` §14.4 and
//! §11's harass-lock row):
//!
//! - a TIMED hold asks `may_release` at its deadline — a veto extends
//!   it, asked again every sweep, and the first "yes" ends it;
//! - a veto still standing at `detach + max_detach_hold` is overridden
//!   (toward the hold's own `ExpireTo`) and the room warns ONCE;
//! - the same ceiling bounds an UNTIMED (combat-held) hold;
//! - a logic that never vetoes ends every hold exactly where it did
//!   before the veto was asked at a deadline — and the ceiling never
//!   shortens a grace;
//! - the config reads literally: `None` = no ceiling, `Some(ZERO)` = no
//!   extension, the default is ten minutes from the DETACH.

use super::*;

mod rig;
use rig::Rig;

const GRACE: Duration = Duration::from_secs(20);
const CEILING: Duration = Duration::from_secs(600);
const SEC: Duration = Duration::from_secs(1);

fn timed(to: ExpireTo) -> Detach {
    Detach::Hold {
        grace: Some(GRACE),
        to,
    }
}

fn untimed(to: ExpireTo) -> Detach {
    Detach::Hold { grace: None, to }
}

/// "Log out 20 s after the disconnect — but not mid-fight."
#[test]
fn a_veto_extends_a_timed_hold_past_its_deadline_until_it_lifts() {
    let mut r = Rig::new(timed(ExpireTo::Despawn), Some(CEILING));
    let p = r.join_and_detach(1);
    r.veto.set(true);
    r.step();
    assert_eq!(r.veto.asks(), 0, "not asked while the grace runs");

    r.age(p, GRACE);
    for _ in 0..5 {
        r.step();
    }
    assert!(r.held(p), "the veto holds it past its deadline");
    assert_eq!(r.veto.asks(), 5, "asked once per sweep past the deadline");
    assert_eq!(r.counts(), (0, 0, 0), "nothing ended, nothing forced");

    r.veto.set(false);
    r.step();
    assert!(!r.actor.conns.contains_key(&p), "released on the next ask");
    assert_eq!(r.ended(), vec![(p, ExpireTo::Despawn)]);
    assert_eq!(r.counts(), (1, 0, 0), "an ordinary end, not a forced one");
}

/// The harass-lock bound: however long the veto stands, the hold ends at
/// the ceiling — measured from the detach — and the room says so once.
#[test]
fn a_standing_veto_is_overridden_at_the_ceiling_with_one_warning() {
    let mut r = Rig::new(timed(ExpireTo::Despawn), Some(CEILING));
    let (a, b) = (r.join_and_detach(1), r.join_and_detach(2));
    r.veto.set(true);
    for p in [a, b] {
        r.age(p, CEILING - SEC);
    }
    r.step();
    assert!(
        r.held(a) && r.held(b),
        "past the grace, short of the ceiling"
    );
    assert_eq!(r.counts(), (0, 0, 0));

    for p in [a, b] {
        r.age(p, SEC);
    }
    r.step();
    assert!(r.actor.conns.is_empty(), "both forced out at the ceiling");
    let mut ended = r.ended();
    ended.sort_by_key(|(p, _)| p.0);
    assert_eq!(ended, vec![(a, ExpireTo::Despawn), (b, ExpireTo::Despawn)]);
    assert_eq!(r.counts(), (2, 0, 1), "one warning for two forced ends");

    let c = r.join_and_detach(3);
    r.age(c, CEILING);
    r.step();
    assert!(!r.held(c), "the ceiling keeps working after the warning");
    assert_eq!(r.counts(), (3, 0, 1), "…and the room stays at one warning");
}

/// The untimed-hold decision: a combat-held park is bounded by the same
/// ceiling (the trusted-`may_release` promise is no longer the only
/// thing between a stuck veto and a slot held forever).
#[test]
fn an_untimed_hold_is_bounded_by_the_same_ceiling() {
    let mut r = Rig::new(untimed(ExpireTo::AiHandover), Some(CEILING));
    let p = r.join_and_detach(1);
    r.veto.set(true);
    r.step();
    assert!(r.held(p), "combat-held from the first sweep");
    assert_eq!(r.veto.asks(), 1, "asked from the first sweep");

    r.age(p, CEILING - SEC);
    r.step();
    assert!(r.held(p), "short of the ceiling the veto still holds");
    r.age(p, SEC);
    r.step();
    assert!(
        r.actor.conns[&p].bot_fed,
        "forced toward its ExpireTo: the bot"
    );
    assert_eq!(r.ended(), vec![(p, ExpireTo::AiHandover)]);
    assert_eq!(r.counts(), (0, 1, 1));

    let asks = r.veto.asks();
    r.step();
    r.step();
    assert_eq!(r.veto.asks(), asks, "a bot-fed row is never asked again");
}

/// Point 4 of the change: without a veto nothing moves — a timed hold
/// ends on the first sweep at its deadline (not before), an untimed one
/// on the first sweep, and a grace longer than the ceiling is honoured.
#[test]
fn a_logic_that_never_vetoes_ends_every_hold_where_it_did() {
    let mut r = Rig::new(timed(ExpireTo::Despawn), Some(CEILING));
    let p = r.join_and_detach(1);
    r.age(p, GRACE - SEC);
    r.step();
    assert!(r.held(p), "not before the deadline");
    r.age(p, SEC);
    r.step();
    assert!(!r.actor.conns.contains_key(&p), "on the deadline's sweep");
    assert_eq!(r.ended(), vec![(p, ExpireTo::Despawn)]);
    assert_eq!(r.counts(), (1, 0, 0));

    let long = Duration::from_secs(3600);
    let hold = Detach::Hold {
        grace: Some(long),
        to: ExpireTo::AiHandover,
    };
    let mut r = Rig::new(hold, Some(CEILING));
    let p = r.join_and_detach(1);
    r.age(p, CEILING + SEC);
    r.step();
    assert!(r.held(p), "the ceiling never shortens a grace");
    r.age(p, long - CEILING - SEC);
    r.step();
    assert!(r.actor.conns[&p].bot_fed, "the grace ends it, unforced");
    assert_eq!(r.counts(), (0, 1, 0));

    let mut r = Rig::new(untimed(ExpireTo::Despawn), Some(CEILING));
    let p = r.join_and_detach(1);
    r.step();
    assert!(!r.actor.conns.contains_key(&p), "untimed: the first sweep");
    assert_eq!(r.counts(), (1, 0, 0));
}

/// The ceiling's config: a finite default measured from the DETACH;
/// `None` = no ceiling; an unrepresentable one is none either (no
/// overflow panic in the actor).
#[test]
fn the_ceiling_defaults_to_ten_minutes_from_the_detach() {
    assert_eq!(DEFAULT_MAX_DETACH_HOLD, CEILING);
    assert_eq!(RoomConfig::default().max_detach_hold, Some(CEILING));

    let before = Instant::now();
    let mut r = Rig::new(timed(ExpireTo::Despawn), Some(CEILING));
    let p = r.join_and_detach(1);
    let after = Instant::now();
    let c = r.actor.conns[&p].detach_ceiling.expect("a ceiling");
    assert!(
        before + CEILING <= c && c <= after + CEILING,
        "detach + 10 min"
    );

    let mut r = Rig::new(untimed(ExpireTo::Despawn), None);
    let p = r.join_and_detach(1);
    r.veto.set(true);
    assert_eq!(r.actor.conns[&p].detach_ceiling, None, "None: no ceiling");
    for _ in 0..5 {
        r.step();
    }
    assert!(
        r.held(p),
        "…so a standing veto holds (the old untimed rule)"
    );
    assert_eq!(r.counts(), (0, 0, 0));

    let mut r = Rig::new(timed(ExpireTo::Despawn), Some(Duration::MAX));
    let p = r.join_and_detach(1);
    assert_eq!(r.actor.conns[&p].detach_ceiling, None, "too far: none");
}

/// `Some(ZERO)` is literal — no extension: a veto is overridden the
/// first time it is asked, which puts a timed hold's end back on its
/// deadline (as before the veto was asked there) and never before it.
#[test]
fn a_zero_ceiling_allows_no_extension() {
    let mut r = Rig::new(timed(ExpireTo::Despawn), Some(Duration::ZERO));
    let p = r.join_and_detach(1);
    r.veto.set(true);
    r.step();
    assert!(r.held(p), "the grace itself still runs");
    r.age(p, GRACE);
    r.step();
    assert!(!r.actor.conns.contains_key(&p), "forced at the deadline");
    assert_eq!(r.counts(), (1, 0, 1));
}
