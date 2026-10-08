//! The subscriber list: interest, per-subscriber numbers, every ending
//! counted under the subscriber's name.

use super::*;

fn online(n: &str) -> EngineEvent {
    EngineEvent::Online {
        identity: Identity::new(n),
    }
}

#[test]
fn interest_filters_and_numbers_are_per_subscriber() {
    let mut subs = EngineSubscribers::new();
    let mut joins = subs.subscribe("joins", Interest::JOINED, 8);
    let mut presence = subs.subscribe("presence", Interest::PRESENCE, 8);
    assert!(!subs.is_empty());
    subs.emit(&online("a"));
    subs.emit(&EngineEvent::Joined {
        identity: Identity::new("a"),
        room: RoomId(4),
    });
    subs.emit(&online("b"));
    let n = joins.try_recv().expect("a join");
    assert_eq!(n.seq, 1);
    assert_eq!(n.event.identity().as_str(), "a");
    assert!(joins.try_recv().is_err(), "joins only");
    let seqs: Vec<u64> = std::iter::from_fn(|| presence.try_recv().ok())
        .map(|n| n.seq)
        .collect();
    assert_eq!(seqs, [1, 2]);
    let counts: Vec<(String, u64)> = subs
        .sources()
        .iter()
        .map(|(n, s)| (n.to_string(), s.counts().engine_events_sent))
        .collect();
    assert_eq!(counts, [("joins".into(), 1), ("presence".into(), 2)]);
}

/// A full subscriber loses the event, counted under its name; the number
/// is spent (the next delivered one shows the gap); a gone one is counted
/// apart; one name used twice is one set of counters.
#[test]
fn a_full_or_gone_subscriber_is_counted_by_name() {
    let mut subs = EngineSubscribers::new();
    let mut slow = subs.subscribe("slow", Interest::ONLINE, 1);
    let gone = subs.subscribe("gone", Interest::ALL, 4);
    drop(gone);
    let _twin = subs.subscribe("slow", Interest::OFFLINE, 4);
    for n in ["a", "b", "c"] {
        subs.emit(&online(n));
    }
    subs.emit(&EngineEvent::Offline {
        identity: Identity::new("a"),
    });
    let sources = subs.sources();
    assert_eq!(sources.len(), 2, "one source per name");
    let c = sources[0].1.counts();
    assert_eq!(sources[0].0.as_ref(), "slow");
    assert_eq!(
        (
            c.engine_events_sent,
            c.engine_events_refused_full,
            c.engine_events_refused_gone
        ),
        (2, 2, 0),
        "one online + the twin's offline sent, two onlines refused"
    );
    let g = sources[1].1.counts();
    assert_eq!(g.engine_events_refused_gone, 4);
    assert_eq!(slow.try_recv().expect("the first").seq, 1);
    subs.emit(&online("d"));
    assert_eq!(slow.try_recv().expect("after the gap").seq, 4);
}

#[test]
fn interest_is_a_bit_set() {
    let i = Interest::ONLINE | Interest::LEFT;
    assert!(i.has(Interest::LEFT) && !i.has(Interest::JOINED));
    assert!(Interest::PRESENCE.has(Interest::RESUMED) && !Interest::PRESENCE.has(Interest::JOINED));
    assert!(!Interest::NONE.has(Interest::ALL));
    assert_eq!(Interest::ALL.bits(), u32::MAX);
    let n = EngineNotice {
        seq: 3,
        event: online("x"),
    };
    assert_eq!(EngineEvent::from(n), online("x"));
}
