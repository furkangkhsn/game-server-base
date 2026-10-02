//! The key-phase policy (B5b), sans-IO on a hand-driven clock: the timer
//! and the record count end a phase, the seal core's distance rule holds
//! a short one, the reliable band's ACK confirms only what it proves (a
//! frame's FIRST send, below the cumulative point), a peer that never
//! confirms is counted and never stalls the session, and the peer's
//! opener follows across a boundary with reordering.

use std::time::Instant;

use super::*;
use crate::seal::{Accept, Initiator, Msg1, Opener, REKEY_MIN_DISTANCE, ResetToken, StaticKey};
use crate::udp::sealed::rekey::REKEY_RETRY;

const W: u64 = REKEY_MIN_DISTANCE;

/// A server → client send half under `policy` (started at `t0`) and the
/// client's opener.
fn halves(policy: RekeyPolicy, t0: Instant) -> (SendHalf, Opener) {
    let server = StaticKey::generate().unwrap();
    let mut ini = Initiator::new(&server.public(), b"ctx", &[]).unwrap();
    let accept = Accept {
        cid: 1,
        reset_token: ResetToken::from_bytes([0; 16]),
    };
    let r = Msg1::parse(ini.msg1())
        .unwrap()
        .cookie_verified(&server, b"ctx", &accept)
        .unwrap();
    let (_, client) = ini.finish(&r.msg2).unwrap();
    let (sealer, _) = r.session.into_halves();
    let (_, opener) = client.into_halves();
    (SendHalf::new(sealer, policy, t0), opener)
}

/// One record at `now`: `(counter, its key phase bit, the datagram)`.
fn rec(s: &mut SendHalf, now: Instant) -> (u64, u8, Vec<u8>) {
    let mut d = Vec::new();
    let c = s.seal(b"x", &mut d, now).expect("sealed");
    (c, d[0] & crate::seal::wire::KIND_PHASE_BIT, d)
}

/// A REL frame `seq` sealed now (its first send), then its ACK.
fn confirm(s: &mut SendHalf, seq: u32, now: Instant) {
    rec(s, now);
    s.sent_rel(seq);
    s.on_ack(seq + 1);
}

fn ms(n: u64) -> Duration {
    Duration::from_millis(n)
}

/// The timer: the phase's first `after` passes with no rekey; the first
/// record past it (distance and confirmation given) is the next
/// generation's, and the client's opener follows it.
#[test]
fn the_timer_ends_a_confirmed_phase() {
    let t0 = Instant::now();
    let policy = RekeyPolicy {
        after: Duration::from_secs(1),
        after_records: u64::MAX,
    };
    let (mut s, mut o) = halves(policy, t0);
    confirm(&mut s, 1, t0);
    for _ in 1..W {
        rec(&mut s, t0 + ms(500));
    }
    let (_, phase, d) = rec(&mut s, t0 + ms(999));
    assert_eq!((phase, s.generation()), (0, 0), "not yet");
    o.open(&d).unwrap();
    let (c, phase, d) = rec(&mut s, t0 + ms(1000));
    assert_eq!((phase, s.generation(), s.rekeys), (1, 1, 1));
    assert_eq!(c, W + 1, "the counter runs on across phases");
    assert_eq!(o.open(&d).unwrap().counter, c);
    assert_eq!(o.generation(), 1, "the opener followed");
    // The new phase's own clock: no second rekey before another second.
    confirm(&mut s, 2, t0 + ms(1500));
    for _ in 0..W {
        rec(&mut s, t0 + ms(1900));
    }
    assert_eq!(s.generation(), 1);
    rec(&mut s, t0 + ms(2000));
    assert_eq!(s.generation(), 2);
}

/// The record count: a phase seals at most `after_records` records.
#[test]
fn the_record_count_ends_a_confirmed_phase() {
    let t0 = Instant::now();
    let policy = RekeyPolicy {
        after: Duration::MAX,
        after_records: 2 * W,
    };
    let (mut s, _) = halves(policy, t0);
    confirm(&mut s, 1, t0);
    for _ in 1..2 * W {
        assert_eq!(rec(&mut s, t0).1, 0);
    }
    let (c, phase, _) = rec(&mut s, t0);
    assert_eq!((c, phase, s.rekeys), (2 * W, 1, 1));
}

/// The seal core's distance rule holds a policy that is always due: a
/// phase is never shorter than REKEY_MIN_DISTANCE records, and the
/// deferral for distance is not counted.
#[test]
fn a_phase_is_never_shorter_than_the_distance_rule() {
    let t0 = Instant::now();
    let policy = RekeyPolicy {
        after: Duration::ZERO,
        after_records: 1,
    };
    let (mut s, mut o) = halves(policy, t0);
    let mut phases = vec![(0u8, 0u64)]; // (phase bit, records in it)
    for seq in 1..=(3 * W as u32 + 10) {
        let (c, phase, d) = rec(&mut s, t0);
        s.sent_rel(seq);
        s.on_ack(seq + 1);
        o.open(&d).unwrap_or_else(|r| panic!("record {c}: {r:?}"));
        match phases.last_mut() {
            Some((p, n)) if *p == phase => *n += 1,
            _ => phases.push((phase, 1)),
        }
    }
    let lens: Vec<u64> = phases.iter().map(|p| p.1).collect();
    assert_eq!(lens, vec![W, W, W, 10], "{lens:?}");
    assert_eq!((s.rekeys, s.unconfirmed), (3, 0));
    assert_eq!(o.generation(), 3);
}

/// A peer that never confirms: the session seals on under its key, each
/// due attempt is counted at most once per REKEY_RETRY, and the first
/// confirmation rekeys at once.
#[test]
fn a_never_confirming_peer_is_counted_and_never_stalls() {
    let t0 = Instant::now();
    let policy = RekeyPolicy {
        after: Duration::from_secs(1),
        after_records: u64::MAX,
    };
    let (mut s, mut o) = halves(policy, t0);
    for i in 0..W {
        let (_, _, d) = rec(&mut s, t0 + ms(i % 900));
        s.sent_rel(i as u32 + 1); // REL frames, never acknowledged
        o.open(&d).unwrap();
    }
    for step in 0..600 {
        let now = t0 + Duration::from_secs(1) + ms(100 * step);
        let (_, phase, d) = rec(&mut s, now);
        assert_eq!(phase, 0, "no confirmation, no rekey");
        assert!(o.open(&d).is_ok(), "the session goes on");
    }
    // Due from 1 s to 60.9 s: counted at 1, 11, 21, 31, 41, 51 s.
    assert_eq!((s.rekeys, s.unconfirmed), (0, 6));
    assert_eq!(REKEY_RETRY, Duration::from_secs(10));
    // The peer acknowledges the first frame: the next record rekeys.
    s.on_ack(2);
    let (_, phase, d) = rec(&mut s, t0 + Duration::from_secs(61));
    assert_eq!((phase, s.rekeys, s.unconfirmed), (1, 1, 6));
    o.open(&d).unwrap();
}

/// What an ACK proves: the frames BELOW its cumulative point reached the
/// peer — not the frame it names next — and a frame confirms the phase
/// of its FIRST send only (a re-send gets a new counter in the current
/// phase, but the ACK may answer the first copy).
#[test]
fn an_ack_confirms_only_first_sends_below_its_point() {
    let t0 = Instant::now();
    let policy = RekeyPolicy {
        after: Duration::ZERO,
        after_records: u64::MAX,
    };
    let (mut s, _) = halves(policy, t0);
    // Frame 1 is sent in phase 0 and acknowledged: confirmed.
    confirm(&mut s, 1, t0);
    // Frame 2's first send is in phase 0 too, then the phase ends.
    rec(&mut s, t0);
    s.sent_rel(2);
    for _ in 2..W {
        rec(&mut s, t0);
    }
    assert_eq!(rec(&mut s, t0).1, 1, "rekeyed: phase 1");
    // Frame 2 re-sent in phase 1 (re-sends are not `sent_rel`), frame 3
    // first sent in phase 1.
    rec(&mut s, t0);
    rec(&mut s, t0);
    s.sent_rel(3);
    for _ in 0..W {
        rec(&mut s, t0);
    }
    // ACK 3: frames 1 and 2 arrived — frame 2's first send was phase 0,
    // so phase 1 is NOT confirmed; frame 3 is not covered (the peer
    // expects it next).
    s.on_ack(3);
    let later = t0 + REKEY_RETRY;
    let before = s.unconfirmed;
    assert_eq!(rec(&mut s, later).1, 1, "still phase 1");
    assert_eq!(s.unconfirmed, before + 1);
    // ACK 4 covers frame 3, first sent in phase 1: confirmed.
    s.on_ack(4);
    assert_eq!(rec(&mut s, later).1, 0, "phase 2");
    assert_eq!(s.rekeys, 2);
}

/// Records reordered across a policy-driven boundary still open: the
/// opener's grace for the previous key.
#[test]
fn reordering_across_a_policy_boundary_still_opens() {
    let t0 = Instant::now();
    let policy = RekeyPolicy {
        after: Duration::from_secs(1),
        after_records: u64::MAX,
    };
    let (mut s, mut o) = halves(policy, t0);
    confirm(&mut s, 1, t0);
    let old: Vec<Vec<u8>> = (1..W).map(|_| rec(&mut s, t0).2).collect();
    let new: Vec<Vec<u8>> = (0..8).map(|_| rec(&mut s, t0 + ms(1000)).2).collect();
    assert_eq!(s.rekeys, 1);
    let held = old.len() - 6;
    for d in &old[..held] {
        o.open(d).unwrap();
    }
    for d in &new[3..] {
        o.open(d).expect("a new-phase record ahead of the old ones");
    }
    assert!(o.holds_previous_key());
    for d in old[held..].iter().chain(&new[..3]) {
        o.open(d)
            .expect("the grace: late old-phase and early new-phase records");
    }
    assert_eq!(o.forged(), 0);
}
