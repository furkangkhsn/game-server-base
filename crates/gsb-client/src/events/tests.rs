//! The sans-IO event table: typed delivery, the numbers and their gaps,
//! streams, the bounded queue, and the ledger of its counters.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use gsb_protocol::event::{ClientEvent, Dest, Event};

use super::*;

#[derive(Clone, PartialEq, prost::Message)]
struct Chat {
    #[prost(string, tag = "1")]
    text: String,
}
impl Event for Chat {
    const OP: u16 = 2002;
    const TO: Dest = Dest::Client;
}
impl ClientEvent for Chat {}

#[derive(Clone, PartialEq, prost::Message)]
struct Balance {
    #[prost(uint64, tag = "1")]
    gold: u64,
}
impl Event for Balance {
    const OP: u16 = 2005;
    const TO: Dest = Dest::Both;
}
impl ClientEvent for Balance {}

/// A `PUSH` frame: `body` from `publisher`, number `seq` in `stream`.
fn frame<E: Event>(publisher: u32, stream: u64, seq: u64, body: &E) -> FrameBody {
    let p = base::Push {
        publisher,
        seq,
        op: u32::from(E::OP),
        body: body.to_wire(),
        stream,
    };
    FrameBody::new(op::base::PUSH, p.encode_to_vec())
}

fn chat(t: &str) -> Chat {
    Chat { text: t.into() }
}

/// What every push ended as adds up to what was received.
fn ledger_holds(e: &Events) {
    let c = e.counters();
    assert_eq!(
        c.received,
        c.stale + c.handled + c.taken + c.dropped_unread + c.body_undecodable + e.queued() as u64,
        "{c:?}, queued {}",
        e.queued()
    );
}

#[test]
fn only_push_frames_are_consumed() {
    let mut e = Events::new();
    assert!(!e.ingest(&FrameBody::new(op::base::HEARTBEAT_ACK, Vec::new())));
    assert!(e.ingest(&frame(1, 1, 1, &chat("hi"))));
    assert_eq!(e.counters().received, 1);
}

#[test]
fn queued_events_are_taken_by_type_in_order() {
    let mut e = Events::new();
    e.ingest(&frame(1, 1, 1, &chat("a")));
    e.ingest(&frame(2, 1, 1, &Balance { gold: 7 }));
    e.ingest(&frame(1, 1, 2, &chat("b")));
    let chats = e.take::<Chat>();
    assert_eq!(
        chats
            .iter()
            .map(|p| p.body.text.as_str())
            .collect::<Vec<_>>(),
        ["a", "b"]
    );
    assert_eq!(
        (chats[1].publisher, chats[1].seq, chats[1].stream),
        (1, 2, 1)
    );
    assert!(e.take::<Chat>().is_empty());
    assert_eq!(e.queued(), 1, "the other type stays");
    let b = e.take::<Balance>();
    assert_eq!(b[0].body.gold, 7);
    assert_eq!(e.counters().taken, 3);
    assert!(e.take_gaps().is_empty());
    ledger_holds(&e);
}

#[test]
fn a_listener_is_called_and_its_type_never_queues() {
    let mut e = Events::new();
    let heard = Arc::new(AtomicU64::new(0));
    let h = heard.clone();
    e.on::<Chat>(move |p| {
        assert_eq!(p.body.text, "hi");
        h.fetch_add(p.seq, Ordering::Relaxed);
    });
    e.ingest(&frame(1, 1, 1, &chat("hi")));
    e.ingest(&frame(1, 1, 2, &chat("hi")));
    e.ingest(&frame(1, 1, 3, &Balance { gold: 1 }));
    assert_eq!(heard.load(Ordering::Relaxed), 3);
    assert_eq!(e.queued(), 1);
    assert!(e.take::<Chat>().is_empty());
    assert_eq!(e.counters().handled, 2);
    ledger_holds(&e);
}

#[test]
fn a_skipped_number_is_a_gap_per_publisher() {
    let mut e = Events::new();
    e.ingest(&frame(1, 4, 1, &chat("a")));
    e.ingest(&frame(2, 4, 1, &chat("x")));
    e.ingest(&frame(1, 4, 4, &chat("d")));
    e.ingest(&frame(2, 4, 2, &chat("y")));
    assert_eq!(
        e.take_gaps(),
        [Gap {
            publisher: 1,
            stream: 4,
            missed: 2,
            seq: 4
        }]
    );
    assert!(e.take_gaps().is_empty(), "handed out once");
    let c = e.counters();
    assert_eq!((c.gaps, c.missed), (1, 2));
    // The events that did arrive are all delivered.
    assert_eq!(e.take::<Chat>().len(), 4);
    ledger_holds(&e);
}

#[test]
fn a_first_number_past_one_is_a_gap() {
    let mut e = Events::new();
    e.ingest(&frame(3, 1, 3, &chat("c")));
    assert_eq!(e.take_gaps()[0].missed, 2);
}

#[test]
fn a_new_stream_restarts_every_count_without_a_gap() {
    let mut e = Events::new();
    e.ingest(&frame(1, 1, 1, &chat("a")));
    e.ingest(&frame(1, 1, 2, &chat("b")));
    e.ingest(&frame(2, 1, 5, &chat("z")));
    e.take_gaps();
    // A new session after being offline: the server's new row.
    e.ingest(&frame(1, 2, 1, &chat("c")));
    e.ingest(&frame(2, 2, 1, &chat("w")));
    assert!(e.take_gaps().is_empty(), "no false gap");
    assert_eq!(e.stream(), Some(2));
    let c = e.counters();
    assert_eq!((c.restarted, c.stale), (1, 0));
    // The first stream is not a restart; a gap in the new one is seen.
    e.ingest(&frame(1, 2, 3, &chat("e")));
    assert_eq!(e.take_gaps()[0].stream, 2);
    ledger_holds(&e);
}

#[test]
fn an_old_number_is_stale_and_dropped() {
    let mut e = Events::new();
    e.ingest(&frame(1, 1, 1, &chat("a")));
    e.ingest(&frame(1, 1, 2, &chat("b")));
    e.ingest(&frame(1, 1, 2, &chat("b again")));
    e.ingest(&frame(1, 1, 1, &chat("a again")));
    assert_eq!(e.counters().stale, 2);
    assert_eq!(e.take::<Chat>().len(), 2);
    ledger_holds(&e);
}

#[test]
fn the_full_queue_drops_its_oldest_and_counts_it() {
    let mut e = Events::with_capacity(2);
    for (i, t) in ["a", "b", "c", "d"].into_iter().enumerate() {
        e.ingest(&frame(1, 1, i as u64 + 1, &chat(t)));
    }
    assert_eq!(e.counters().dropped_unread, 2);
    let kept: Vec<_> = e.take::<Chat>().into_iter().map(|p| p.body.text).collect();
    assert_eq!(kept, ["c", "d"], "the newest stay");
    ledger_holds(&e);
}

#[test]
fn a_zero_capacity_table_queues_nothing() {
    let mut e = Events::with_capacity(0);
    e.ingest(&frame(1, 1, 1, &chat("a")));
    assert_eq!((e.queued(), e.counters().dropped_unread), (0, 1));
    ledger_holds(&e);
}

#[test]
fn undecodable_envelopes_and_bodies_are_counted_apart() {
    let mut e = Events::new();
    assert!(e.ingest(&FrameBody::new(op::base::PUSH, vec![0xff, 0xff])));
    assert_eq!(e.counters().undecodable, 1);
    assert_eq!(e.counters().received, 0);
    // A body that is no `Balance`: field 1 as a length-delimited string.
    let bad = base::Push {
        publisher: 1,
        seq: 1,
        op: u32::from(Balance::OP),
        body: vec![0x0a, 0x01, b'x'],
        stream: 1,
    };
    e.push(bad.clone());
    assert!(e.take::<Balance>().is_empty());
    e.on::<Balance>(|_| panic!("never decoded"));
    e.push(base::Push { seq: 2, ..bad });
    assert_eq!(e.counters().body_undecodable, 2);
    ledger_holds(&e);
}

#[test]
fn counters_fold_and_name_every_field() {
    let mut a = EventCounters {
        received: 1,
        missed: 2,
        ..Default::default()
    };
    a.add(&EventCounters {
        received: 3,
        dropped_unread: 4,
        ..Default::default()
    });
    let f = a.fields();
    assert_eq!(f[0], ("received", 4));
    assert_eq!(f[3], ("missed", 2));
    assert_eq!(f[8], ("dropped_unread", 4));
}
