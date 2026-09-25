//! The reassembly BOUNDS (`docs/SECURITY.md` §4.1): the fragment
//! ceiling and malformed headers, the partial-message cap, the memory
//! cap, and the id wrap the slot rule depends on. A child of the rules'
//! tests, so it shares their helpers.

use super::*;

/// Different slots hold different messages at once: up to FRAG_SLOTS
/// partial messages coexist, and the next one evicts only the one in its
/// own slot.
#[test]
fn partial_messages_are_capped_at_the_slot_count() {
    let (mut r, mut s, now) = fresh();
    let bodies: Vec<Vec<u8>> = (0..=FRAG_SLOTS as u8)
        .map(|k| body(1201, 3000, k))
        .collect();
    let all: Vec<Vec<Vec<u8>>> = bodies
        .iter()
        .enumerate()
        .map(|(id, b)| frags(id as u16, b))
        .collect();
    for fs in &all[..FRAG_SLOTS] {
        assert_eq!(feed(&mut r, &mut s, &fs[0], now), None);
    }
    assert_eq!(s.frag_dropped_incomplete, 0, "four partials fit");
    assert_eq!(feed(&mut r, &mut s, &all[FRAG_SLOTS][0], now), None);
    assert_eq!(s.frag_dropped_incomplete, 1, "the fifth evicts one");
    // Messages 1..=3 are intact and still complete.
    for (id, fs) in all.iter().enumerate().take(FRAG_SLOTS).skip(1) {
        let mut got = None;
        for d in &fs[1..] {
            got = feed(&mut r, &mut s, d, now).or(got);
        }
        assert_eq!(got.as_ref(), Some(&bodies[id]), "message {id}");
    }
}

/// The fragment ceiling on both sides: the split refuses a message over
/// FRAG_MAX_COUNT fragments (the writer's drop+count path), and the
/// reassembly refuses a header claiming one — or an impossible header.
#[test]
fn the_fragment_ceiling_and_malformed_headers() {
    let chunk = DEFAULT_MAX_DATAGRAM_BYTES - FRAG_HEADER;
    let at_ceiling = vec![0u8; FRAG_MAX_COUNT * chunk];
    let n = split(0, &at_ceiling, DEFAULT_MAX_DATAGRAM_BYTES).map(|v| v.len());
    assert_eq!(n, Some(FRAG_MAX_COUNT), "exactly at the ceiling: sent");
    let over = vec![0u8; FRAG_MAX_COUNT * chunk + 1];
    assert!(split(0, &over, DEFAULT_MAX_DATAGRAM_BYTES).is_none());
    assert!(
        split(0, &[0u8; 10], FRAG_HEADER).is_none(),
        "no room for a chunk"
    );

    let (mut r, mut s, now) = fresh();
    let bad = [
        vec![KIND_FRAG, 0, 0, 0, 17, 1], // count past the ceiling
        vec![KIND_FRAG, 0, 0, 0, 1, 1],  // a single "fragment"
        vec![KIND_FRAG, 0, 0, 2, 2, 1],  // index >= count
        vec![KIND_FRAG, 0, 0, 0, 2],     // no chunk
    ];
    for d in &bad {
        assert_eq!(feed(&mut r, &mut s, d, now), None);
    }
    assert_eq!(s.frag_rejected, bad.len() as u64);
    assert_eq!(r.buffered(), 0, "nothing refused is held");
    // The same message cannot change its count mid-way.
    assert_eq!(feed(&mut r, &mut s, &[KIND_FRAG, 5, 0, 0, 3, 1], now), None);
    assert_eq!(feed(&mut r, &mut s, &[KIND_FRAG, 5, 0, 1, 4, 1], now), None);
    assert_eq!(s.frag_rejected, bad.len() as u64 + 1);
}

/// The memory bound: chunk bytes held across the slots never exceed
/// FRAG_MEM_CAP; the oldest OTHER partial is evicted to make room.
#[test]
fn reassembly_memory_is_capped() {
    let (mut r, mut s, t0) = fresh();
    let chunk = [7u8; 2000];
    let frag = |id: u16, index: u8| {
        let mut d = vec![KIND_FRAG];
        d.extend_from_slice(&id.to_le_bytes());
        d.extend_from_slice(&[index, 16]);
        d.extend_from_slice(&chunk);
        d
    };
    // Two partials of 15 × 2000 bytes: 60 000 held.
    for id in 0..2u16 {
        let at = t0 + Duration::from_millis(u64::from(id));
        for i in 0..15 {
            feed(&mut r, &mut s, &frag(id, i), at);
        }
    }
    assert_eq!(r.buffered(), 60_000);
    assert_eq!(s.frag_dropped_incomplete, 0);
    // A third message: its third chunk would pass the cap → the OLDEST
    // other partial (message 0) goes.
    let t1 = t0 + Duration::from_millis(5);
    feed(&mut r, &mut s, &frag(2, 0), t1);
    feed(&mut r, &mut s, &frag(2, 1), t1);
    assert_eq!(s.frag_dropped_incomplete, 0, "64 000 bytes still fit");
    feed(&mut r, &mut s, &frag(2, 2), t1);
    assert_eq!(s.frag_dropped_incomplete, 1);
    assert!(r.buffered() <= FRAG_MEM_CAP);
    assert_eq!(r.buffered(), 30_000 + 6_000, "messages 1 and 2 remain");
    // Message 1 (not the evicted one) still completes.
    let whole = feed(&mut r, &mut s, &frag(1, 15), t1).expect("message 1 completes");
    assert_eq!(whole.len(), 16 * 2000);
}

/// The message id wraps: 0 is newer than 65 535 − 3, so the wrap evicts
/// the old partial in the shared slot rather than refusing the new one.
#[test]
fn message_ids_wrap() {
    let (mut r, mut s, now) = fresh();
    let old = frags(u16::MAX - 3, &body(1101, 2000, 11));
    assert_eq!(feed(&mut r, &mut s, &old[0], now), None);
    let b = body(1101, 2000, 12);
    let mut got = None;
    for d in &frags(0, &b) {
        got = feed(&mut r, &mut s, d, now).or(got);
    }
    assert_eq!(got, Some(b));
    assert_eq!(s.frag_dropped_incomplete, 1);
}
