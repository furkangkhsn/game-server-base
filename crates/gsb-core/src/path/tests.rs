//! The path state's own rules: the budget, the change rule, the
//! latest-wins signal and the internal carry layout.

use std::time::Duration;

use super::carry::{decode, encode};
use super::*;
use crate::id::{ConnectionId, PlayerId};

fn paced(rate: u32) -> PathState {
    PathState {
        phase: PathPhase::Paced,
        rate: Some(rate),
        ..Default::default()
    }
}

#[test]
fn the_budget_is_the_paced_rate_over_the_period() {
    let tick = Duration::from_secs(1) / 30;
    assert_eq!(paced(30_000).budget(tick), Some(999));
    assert_eq!(paced(30_000).budget(Duration::from_millis(50)), Some(1_500));
    assert_eq!(PathState::default().budget(tick), None, "open: no limit");
    let suspect = PathState {
        phase: PathPhase::Suspect,
        ..Default::default()
    };
    assert_eq!(suspect.budget(tick), None);
}

#[test]
fn a_state_is_news_on_a_phase_change_or_a_tenth_of_the_rate() {
    let open = PathState::default();
    let suspect = PathState {
        phase: PathPhase::Suspect,
        ..open
    };
    assert!(suspect.moved_from(&open));
    assert!(!open.moved_from(&open));
    // The measurements alone are not news.
    let measured = PathState {
        rtt: Some(Duration::from_millis(80)),
        loss_permille: Some(40),
        demand: Some(9_000),
        queue_delay: Some(Duration::from_millis(30)),
        ..open
    };
    assert!(!measured.moved_from(&open));
    // The rate: 10 % of the OLD rate, either way.
    assert!(!paced(10_999).moved_from(&paced(10_000)));
    assert!(paced(11_000).moved_from(&paced(10_000)));
    assert!(paced(9_000).moved_from(&paced(10_000)));
    assert!(!paced(9_001).moved_from(&paced(10_000)));
    assert!(!paced(10_000).moved_from(&paced(10_000)));
    assert!(!paced(0).moved_from(&paced(0)));
    assert!(paced(1).moved_from(&paced(0)));
    // A rate appearing or going is news within one phase too.
    let unrated = PathState {
        rate: None,
        ..paced(10_000)
    };
    assert!(unrated.moved_from(&paced(10_000)));
    assert!(paced(10_000).moved_from(&unrated));
}

#[test]
fn the_signal_owes_news_and_keeps_only_the_newest() {
    let mut s = PathSignal::default();
    assert_eq!(s.owed(), None, "nothing offered");
    s.offer(PathState::default());
    assert_eq!(
        s.owed(),
        Some(PathState::default()),
        "the first state is news"
    );
    s.delivered();
    assert_eq!(s.owed(), None);
    // Not news: nothing owed.
    s.offer(PathState {
        rtt: Some(Duration::from_millis(5)),
        ..Default::default()
    });
    assert_eq!(s.owed(), None);
    // News that cannot be delivered stays owed; a newer state replaces it.
    s.offer(paced(50_000));
    assert_eq!(s.owed(), Some(paced(50_000)));
    s.offer(paced(51_000));
    assert_eq!(s.owed(), Some(paced(51_000)), "latest wins");
    // A path back where the receiver last saw it owes nothing.
    s.offer(PathState::default());
    assert_eq!(s.owed(), None, "back to the delivered state");
    s.offer(paced(51_000));
    s.delivered();
    assert_eq!(s.owed(), None);
    // Measured against what was DELIVERED, not what was offered.
    s.offer(paced(53_000));
    assert_eq!(s.owed(), None, "under a tenth of 51 000");
    s.offer(paced(56_100));
    assert_eq!(s.owed(), Some(paced(56_100)));
}

#[test]
fn a_new_receiver_is_owed_the_newest_state() {
    let mut s = PathSignal::default();
    s.reset();
    assert_eq!(s.owed(), None, "nothing known, nothing owed");
    s.offer(paced(40_000));
    s.delivered();
    s.offer(paced(40_100));
    s.reset();
    assert_eq!(s.owed(), Some(paced(40_100)));
}

#[test]
fn the_carry_round_trips_every_field_and_absence() {
    let full = PathState {
        phase: PathPhase::Paced,
        rate: Some(123_456),
        demand: Some(u32::MAX),
        loss_permille: Some(1_000),
        rtt: Some(Duration::from_micros(80_123)),
        queue_delay: Some(Duration::from_micros(30_001)),
    };
    assert_eq!(decode(&encode(&full)), Some(full));
    for phase in [PathPhase::Open, PathPhase::Suspect] {
        let bare = PathState {
            phase,
            ..Default::default()
        };
        assert_eq!(decode(&encode(&bare)), Some(bare));
    }
    // One field at a time: each presence bit is its own.
    let rtt_only = PathState {
        rtt: Some(Duration::ZERO),
        ..Default::default()
    };
    assert_eq!(decode(&encode(&rtt_only)), Some(rtt_only));
    let demand_only = PathState {
        demand: Some(0),
        ..Default::default()
    };
    assert_eq!(decode(&encode(&demand_only)), Some(demand_only));
    // Durations saturate rather than wrap.
    let long = PathState {
        queue_delay: Some(Duration::from_secs(10_000)),
        ..Default::default()
    };
    let back = decode(&encode(&long)).expect("decodes");
    assert_eq!(
        back.queue_delay,
        Some(Duration::from_micros(u64::from(u32::MAX)))
    );
}

#[test]
fn a_malformed_carry_is_refused() {
    let good = encode(&paced(1));
    assert_eq!(decode(&good[..19]), None, "short");
    let mut long = good.to_vec();
    long.push(0);
    assert_eq!(decode(&long), None, "long");
    let mut phase = good.to_vec();
    phase[0] = 3;
    assert_eq!(decode(&phase), None, "unknown phase");
}

#[test]
fn only_the_marker_reads_as_a_path() {
    let conn = ConnectionId(9);
    let marker = path_action(conn, &paced(7_000));
    assert_eq!(marker.conn, conn);
    assert_eq!(read_path(&marker), Some(Some(paced(7_000))));
    let game = crate::room::Action {
        conn,
        player: PlayerId(0),
        op: gsb_protocol::op::GAME_BAND_START,
        payload: encode(&paced(7_000)),
    };
    assert_eq!(read_path(&game), None, "a game action is never a path");
    let broken = crate::room::Action {
        payload: bytes::Bytes::from_static(b"x"),
        ..path_action(conn, &paced(1))
    };
    assert_eq!(read_path(&broken), Some(None));
}

#[test]
fn the_view_answers_per_member_and_the_empty_view_never() {
    let mut t = PathTable::default();
    let tick = Duration::from_millis(50);
    let v = PathView::default();
    assert_eq!(v.path(PlayerId(1)), None);
    assert_eq!(v.budget(PlayerId(1)), None);
    t.set(PlayerId(1), paced(20_000));
    t.set(PlayerId(2), PathState::default());
    let v = t.view(tick);
    assert_eq!(v.budget(PlayerId(1)), Some(1_000));
    assert_eq!(v.budget(PlayerId(2)), None, "known but open");
    assert_eq!(v.path(PlayerId(2)), Some(PathState::default()));
    assert_eq!(v.path(PlayerId(3)), None);
    assert_eq!(v.period(), Some(tick));
    assert_eq!(t.remove(PlayerId(1)), Some(paced(20_000)));
    assert_eq!(t.len(), 1);
}
