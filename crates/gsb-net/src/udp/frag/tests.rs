//! The reassembly rules, one per test, on the real split and the real
//! reassembly state (no socket: the clock is a parameter, so aging is
//! deterministic). The socket-level round trip through the server writer
//! lives in `udp::tests::frag`.

use super::*;
use bytes::Bytes;

/// The byte string a RAW datagram would carry after its kind byte.
fn body(op: u16, payload_len: usize, seed: u8) -> Vec<u8> {
    let payload: Vec<u8> = (0..payload_len)
        .map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed))
        .collect();
    FrameBody::new(op, Bytes::from(payload)).encode().to_vec()
}

/// Split with the default budget.
fn frags(id: u16, b: &[u8]) -> Vec<Vec<u8>> {
    split(id, b, DEFAULT_MAX_DATAGRAM_BYTES).expect("within the ceiling")
}

fn feed(r: &mut Reassembly, s: &mut UdpClientStats, d: &[u8], now: Instant) -> Option<Vec<u8>> {
    r.accept(d, now, s).map(|f| f.encode().to_vec())
}

fn fresh() -> (Reassembly, UdpClientStats, Instant) {
    (
        Reassembly::default(),
        UdpClientStats::default(),
        Instant::now(),
    )
}

/// Split → reassemble is the identity, and every fragment fits the budget.
#[test]
fn a_large_message_round_trips() {
    let b = body(1101, 5000, 1);
    let fs = frags(7, &b);
    assert_eq!(fs.len(), 4, "5002 bytes / 1467-byte chunks");
    assert!(fs.iter().all(|d| d.len() <= DEFAULT_MAX_DATAGRAM_BYTES));
    assert!(fs.iter().all(|d| d[0] == KIND_FRAG));
    let (mut r, mut s, now) = fresh();
    for d in &fs[..3] {
        assert_eq!(feed(&mut r, &mut s, d, now), None, "not whole yet");
    }
    assert_eq!(feed(&mut r, &mut s, &fs[3], now), Some(b));
    assert_eq!(s.frag_reassembled, 1);
    assert_eq!(r.buffered(), 0, "a delivered message holds no memory");
}

/// Fragments arriving in any order rebuild the same message.
#[test]
fn out_of_order_fragments_reassemble() {
    let b = body(1201, 6000, 2);
    let fs = frags(1, &b);
    let (mut r, mut s, now) = fresh();
    let order = [3usize, 0, 4, 2, 1];
    for &i in &order[..4] {
        assert_eq!(feed(&mut r, &mut s, &fs[i], now), None);
    }
    assert_eq!(feed(&mut r, &mut s, &fs[order[4]], now), Some(b));
}

/// A duplicated fragment is ignored before completion (it does not count
/// twice toward the whole), and a late duplicate after delivery does not
/// deliver the message again.
#[test]
fn duplicate_fragments_neither_complete_nor_redeliver() {
    let b = body(1101, 3000, 3);
    let fs = frags(2, &b);
    assert_eq!(fs.len(), 3);
    let (mut r, mut s, now) = fresh();
    assert_eq!(feed(&mut r, &mut s, &fs[0], now), None);
    assert_eq!(feed(&mut r, &mut s, &fs[0], now), None);
    assert_eq!(feed(&mut r, &mut s, &fs[1], now), None, "two of three");
    assert_eq!(feed(&mut r, &mut s, &fs[2], now), Some(b));
    for d in &fs {
        assert_eq!(feed(&mut r, &mut s, d, now), None, "delivered once only");
    }
    assert_eq!(s.frag_reassembled, 1);
    assert_eq!(s.frag_dropped_incomplete, 0);
}

/// A missing fragment loses exactly its own message: the next message is
/// delivered, and the incomplete one is dropped (and counted) when a
/// newer message takes its slot — never delivered half-built, never
/// resurrected by a straggler.
#[test]
fn a_missing_fragment_drops_exactly_that_message() {
    let (mut r, mut s, now) = fresh();
    let lost = frags(0, &body(1201, 4000, 4));
    for d in [&lost[0], &lost[2]] {
        assert_eq!(feed(&mut r, &mut s, d, now), None);
    }
    let next = body(1201, 4000, 5);
    let mut got = None;
    for d in &frags(1, &next) {
        got = feed(&mut r, &mut s, d, now).or(got);
    }
    assert_eq!(got, Some(next), "the next message is unaffected");
    assert_eq!(s.frag_dropped_incomplete, 0, "not superseded yet");

    // Message 4 lands in message 0's slot: 0 is dropped, not delivered.
    let later = body(1201, 2000, 6);
    let fs = frags(4, &later);
    assert_eq!(feed(&mut r, &mut s, &fs[0], now), None);
    assert_eq!(s.frag_dropped_incomplete, 1, "the superseded partial");
    // The straggler of message 0 is refused, not a new partial.
    assert_eq!(feed(&mut r, &mut s, &lost[1], now), None);
    assert_eq!(s.frag_rejected, 1);
    assert_eq!(feed(&mut r, &mut s, &fs[1], now), Some(later));
    assert_eq!(s.frag_dropped_incomplete, 1);
}

/// A newer message in the same slot evicts an older partial even while
/// the two interleave; the older one's late fragments stay refused.
#[test]
fn an_interleaved_newer_message_evicts_the_older_partial() {
    let (mut r, mut s, now) = fresh();
    let old = frags(9, &body(1101, 3000, 7));
    let new_body = body(1101, 3000, 8);
    let new = frags(9 + FRAG_SLOTS as u16, &new_body);
    assert_eq!(feed(&mut r, &mut s, &old[0], now), None);
    assert_eq!(feed(&mut r, &mut s, &new[0], now), None);
    assert_eq!(feed(&mut r, &mut s, &old[1], now), None);
    assert_eq!(feed(&mut r, &mut s, &new[1], now), None);
    assert_eq!(feed(&mut r, &mut s, &old[2], now), None);
    assert_eq!(feed(&mut r, &mut s, &new[2], now), Some(new_body));
    assert_eq!(s.frag_dropped_incomplete, 1);
    assert_eq!(s.frag_rejected, 2, "both late fragments of the old one");
}

/// A partial message older than FRAG_MAX_AGE is dropped by the next
/// fragment to arrive, whatever its slot.
#[test]
fn a_partial_message_ages_out() {
    let (mut r, mut s, t0) = fresh();
    let fs = frags(0, &body(1101, 3000, 9));
    assert_eq!(feed(&mut r, &mut s, &fs[0], t0), None);
    let other = frags(1, &body(1101, 3000, 10));
    assert_eq!(feed(&mut r, &mut s, &other[0], t0 + FRAG_MAX_AGE), None);
    assert_eq!(s.frag_dropped_incomplete, 0, "exactly at the bound: kept");
    let t1 = t0 + FRAG_MAX_AGE + Duration::from_millis(1);
    assert_eq!(feed(&mut r, &mut s, &other[1], t1), None);
    assert_eq!(s.frag_dropped_incomplete, 1, "past the bound: dropped");
    assert_eq!(
        r.buffered(),
        other[0].len() + other[1].len() - 2 * FRAG_HEADER
    );
}

mod bounds;
