//! The duelists: who (a pure function of the id, pairs together, the
//! fraction held), where (8 m either side of the seam, on their side's
//! waystone row), what they send — and that a bot who is not one sends
//! exactly what it would without the flag.

use std::time::Duration;

use gsb_demo_mmo::codec::to_dm;
use gsb_demo_mmo::mmo::{Attack, Kind, MoveTo, Travel};
use gsb_demo_mmo::op;
use gsb_demo_mmo::world::{ATTACK_RANGE, WAYSTONES};
use prost::Message;

use super::*;
use crate::bot::LoadBot;
use crate::bot::mmo::MmoBot;
use crate::bot::mmo::roster::home_waystone;
use crate::bot::mmo::tests::full;

fn bot(duel_frac: f64) -> MmoBot {
    MmoBot {
        move_ms: Duration::from_millis(150),
        duel_frac,
    }
}

/// None at 0, all at 1; at 0.2 a fifth of the pairs (within one pair),
/// both bots of a pair; the posts sit 8 m either side of x = 0, on the
/// row of the waystone on their side, 16 m from the partner (in reach).
#[test]
fn duelists_are_chosen_by_pair_and_posted_across_the_seam() {
    assert!((0..1000).all(|id| duel_of(id, 0.0).is_none()));
    assert!((0..1000).all(|id| duel_of(id, 1.0).is_some()));
    let pairs = (0..500u64)
        .filter(|&j| duel_of(2 * j, 0.2).is_some())
        .count();
    assert!((99..=101).contains(&pairs), "{pairs} of 500 pairs");
    for j in 0..500u64 {
        let (w, e) = (duel_of(2 * j, 0.2), duel_of(2 * j + 1, 0.2));
        assert_eq!(w.is_some(), e.is_some(), "pair {j} duels together");
        let (Some(w), Some(e)) = (w, e) else { continue };
        assert_eq!((w.x, e.x, w.z), (-8.0, 8.0, e.z), "pair {j}");
        assert!(e.x - w.x < ATTACK_RANGE);
        for d in [w, e] {
            let [wx, wz] = WAYSTONES[d.waystone];
            assert_eq!((wx < 0.0, wz < 0.0), (d.x < 0.0, d.z < 0.0), "{d:?}");
            assert!((d.z - wz).abs() <= 112.0, "on the waystone's row: {d:?}");
        }
    }
}

/// A duelist whose home waystone (where its saved character starts) is
/// not its side's travels there first, walks to its post, and
/// attacks only a PLAYER across the seam within reach — at the attack
/// rate; a bot that is not a duelist sends, input for input, what it
/// sends without the flag.
#[test]
fn a_duelist_fights_across_the_seam_and_the_others_are_unchanged() {
    let id = (0..)
        .find(|&id| duel_of(id, 0.5).is_some_and(|d| d.waystone != home_waystone(id)))
        .unwrap();
    let post = duel_of(id, 0.5).expect("a duelist");
    let mut c = bot(0.5).client(id);
    c.joined(7);
    let (px, pz) = (to_dm(post.x), to_dm(post.z));
    let across = if post.x < 0.0 { px + 160 } else { px - 160 };
    let records = [
        (7, px, 0, pz, Kind::Player),
        (8, across, 0, pz, Kind::Player), // the foe, 16 m across
        (9, px + px.signum() * 50, 0, pz, Kind::Player), // same side
        (10, -px, 0, pz + 30, Kind::Mob), // a mob across
    ];
    c.apply_snapshot(&full(&records)).expect("applies");
    let (op, first) = c.next_input(Duration::ZERO, 1).expect("an input");
    assert_eq!(op, op::MMO_TRAVEL);
    assert_eq!(
        Travel::decode(&first[..]).unwrap().waystone as usize,
        post.waystone
    );
    let mut attacks = 0;
    for seq in 2..=4001u64 {
        let (op, payload) = c.next_input(Duration::from_millis(seq * 150), seq).unwrap();
        if op == op::MMO_ATTACK {
            assert_eq!(
                Attack::decode(&payload[..]).unwrap().target,
                8,
                "the foe only"
            );
            attacks += 1;
        } else {
            let m = MoveTo::decode(&payload[..]).expect("MoveTo");
            assert_eq!((m.x, m.z, m.seq), (px, pz, seq), "to its post");
        }
    }
    assert!(
        (450..=750).contains(&attacks),
        "at the attack rate: {attacks}"
    );

    let plain = (0..).find(|&id| duel_of(id, 0.5).is_none()).unwrap();
    let (mut a, mut b) = (bot(0.0).client(plain), bot(0.5).client(plain));
    let [wx, wz] = WAYSTONES[0];
    let view = full(&[(7, to_dm(wx), 0, to_dm(wz), Kind::Player)]);
    for c in [&mut a, &mut b] {
        c.joined(7);
        c.apply_snapshot(&view).unwrap();
    }
    for seq in 1..=2000u64 {
        let t = Duration::from_millis(seq * 150);
        assert_eq!(a.next_input(t, seq), b.next_input(t, seq), "input {seq}");
    }
}
