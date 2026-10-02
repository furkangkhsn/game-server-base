//! Stateless reset (B5b): the reset key's derivations, and the reset
//! datagram — always shorter than its trigger, shaped like a small
//! server → client record, refused by the opener before its tail is
//! compared with the token.

use super::*;
use crate::seal::wire::{HEADER_LEN_C2S, KIND_PHASE_BIT, KIND_SEALED, OVERHEAD_S2C, TAG_LEN};

#[test]
fn the_derived_reset_key_follows_the_static_key_and_the_door() {
    let s = StaticKey::from_private([5; KEY_LEN]).unwrap();
    let again = StaticKey::from_private([5; KEY_LEN]).unwrap();
    let other = StaticKey::from_private([6; KEY_LEN]).unwrap();
    let k = ResetKey::derived_from(&s);
    assert_eq!(
        k.token(1),
        ResetKey::derived_from(&again).token(1),
        "a restart with the same static key derives the same key"
    );
    assert_ne!(k.token(1), ResetKey::derived_from(&other).token(1));
    // Not the static key's bytes used as a reset key.
    assert_ne!(k.token(1), ResetKey::from_bytes([5; KEY_LEN]).token(1));
    // Bound to a door: the same door again is the same key, another
    // door (or the unbound key) is not.
    let door = k.for_door(b"127.0.0.1:7000");
    assert_eq!(door.token(9), k.for_door(b"127.0.0.1:7000").token(9));
    assert_ne!(door.token(9), k.for_door(b"127.0.0.1:7001").token(9));
    assert_ne!(door.token(9), k.token(9));
    assert_eq!(format!("{door:?}"), "ResetKey(..)", "never the key");
}

/// Every trigger a demux can read (a c→s record holds at least its
/// 33-byte header and tag) gets a reset strictly shorter than itself,
/// between the bounds; a trigger too short for that gets none.
#[test]
fn a_reset_is_always_shorter_than_its_trigger() {
    let token = ResetKey::from_bytes([3; 32]).token(77);
    let random = [0xFF; RESET_LEN_MAX];
    for trigger in 0..=1472 {
        match reset_datagram(&token, trigger, &random) {
            Some(d) => {
                assert!(d.len() < trigger, "{trigger}: {} B", d.len());
                assert!((RESET_LEN_MIN..=RESET_LEN_MAX).contains(&d.len()));
            }
            None => assert!(trigger <= RESET_LEN_MIN, "{trigger}"),
        }
    }
    assert_eq!(RESET_LEN_MIN, OVERHEAD_S2C + 1);
    let min_trigger = HEADER_LEN_C2S + TAG_LEN;
    assert_eq!(
        reset_datagram(&token, min_trigger, &random).map(|d| d.len()),
        Some(min_trigger - 1)
    );
}

/// The layout: a SEALED kind byte with the random phase bit, a counter
/// below 2^62, the random filler, the token last.
#[test]
fn the_reset_has_a_records_shape_and_ends_with_the_token() {
    let token = ResetKey::from_bytes([3; 32]).token(77);
    for first in [0xFE_u8, 0xFF] {
        let mut random = [0xFF_u8; RESET_LEN_MAX];
        random[0] = first;
        let d = reset_datagram(&token, 100, &random).unwrap();
        assert_eq!(d.len(), RESET_LEN_MAX);
        assert_eq!(d[0], KIND_SEALED | (first & KIND_PHASE_BIT));
        let counter = u64::from_le_bytes(d[1..9].try_into().unwrap());
        assert_eq!(
            counter,
            SEAL_LIMIT - 1,
            "all ones, the top two bits cleared"
        );
        assert!(d[9..d.len() - 16].iter().all(|&b| b == 0xFF), "the filler");
        assert!(token.matches(&d[d.len() - 16..]));
        assert_eq!(reset_tail(&d), Some(&d[d.len() - 16..]));
    }
}

/// What the client sees: the reset is no record — its opener refuses it
/// as forged (the counter reaches the AEAD), and only then does the tail
/// match the session's token. A record, a plaintext datagram, or a
/// datagram outside the reset's sizes has no tail to compare.
#[test]
fn the_opener_refuses_a_reset_and_its_tail_carries_the_token() {
    let (_, mut co, mut ss, _, accept) = pair(5);
    let mut random = [0u8; RESET_LEN_MAX];
    for (i, b) in random.iter_mut().enumerate() {
        *b = (i as u8).wrapping_mul(37) ^ 0x5A;
    }
    let d = reset_datagram(&accept.reset_token, 60, &random).unwrap();
    assert_eq!(co.open(&d), Err(Refusal::Forged));
    assert!(accept.reset_token.matches(reset_tail(&d).unwrap()));
    // A genuine small record of reset size: a tail, not the token.
    let rec = seal_n(&mut ss, 1).remove(0);
    assert!((RESET_LEN_MIN..=RESET_LEN_MAX).contains(&rec.len()));
    assert!(!accept.reset_token.matches(reset_tail(&rec).unwrap()));
    // No tail: too short, too long, not SEALED.
    assert_eq!(reset_tail(&d[..RESET_LEN_MIN - 1]), None);
    assert_eq!(reset_tail(&[d.clone(), vec![0]].concat()), None);
    let mut plain = d.clone();
    plain[0] = 0x01;
    assert_eq!(reset_tail(&plain), None);
}
