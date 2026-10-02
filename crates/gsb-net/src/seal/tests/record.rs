//! The record layer: layout, every refusal by name, the counter limit.

use super::super::wire::{
    COUNTER_LEN, HEADER_LEN_C2S, HEADER_LEN_S2C, KIND_PHASE_BIT, KIND_SEALED, KeyPhase,
    OVERHEAD_C2S, OVERHEAD_S2C, TAG_LEN,
};
use super::*;

#[test]
fn layout_of_both_directions() {
    let (mut cs, _, mut ss, _, _) = pair(0x1122_3344_5566_7788);
    let mut up = Vec::new();
    cs.seal(b"hello", &mut up).unwrap();
    cs.seal(b"hello", &mut up).unwrap(); // appends a second datagram
    let first = &up[..OVERHEAD_C2S + 5];
    assert_eq!(first[0], KIND_SEALED);
    assert_eq!(first[1..9], 0x1122_3344_5566_7788u64.to_le_bytes());
    assert_eq!(first[9..HEADER_LEN_C2S], 0u64.to_le_bytes());
    assert_eq!(
        up[OVERHEAD_C2S + 5 + 9..OVERHEAD_C2S + 5 + HEADER_LEN_C2S],
        1u64.to_le_bytes()
    );
    let mut down = Vec::new();
    assert_eq!(ss.seal(b"", &mut down), Ok(0));
    assert_eq!(down.len(), OVERHEAD_S2C);
    assert_eq!(down[0], KIND_SEALED);
    assert_eq!(down[1..1 + COUNTER_LEN], 0u64.to_le_bytes());
    assert_eq!(
        (OVERHEAD_C2S, OVERHEAD_S2C, HEADER_LEN_S2C, TAG_LEN),
        (33, 25, 9, 16)
    );
}

#[test]
fn header_codec_round_trips_and_refuses_non_sealed() {
    for (h, dir) in [
        (
            Header {
                phase: KeyPhase::One,
                cid: Some(u64::MAX),
                counter: 7,
            },
            Direction::ClientToServer,
        ),
        (
            Header {
                phase: KeyPhase::Zero,
                cid: None,
                counter: 1 << 61,
            },
            Direction::ServerToClient,
        ),
    ] {
        let (b, len) = h.encode();
        let mut d = b[..len].to_vec();
        assert_eq!(Header::decode(&d, dir), None, "no room for a tag");
        d.extend_from_slice(&[0; TAG_LEN]);
        assert_eq!(Header::decode(&d, dir), Some(h));
        d[0] = 0x00; // a plaintext RAW kind
        assert_eq!(Header::decode(&d, dir), None);
        d[0] = KIND_SEALED | 0x02;
        assert_eq!(Header::decode(&d, dir), None);
    }
    assert_eq!(KIND_SEALED | KIND_PHASE_BIT, 0x41);
}

#[test]
fn newest_flag_follows_the_highest_counter() {
    let (mut cs, _, _, mut so, _) = pair(1);
    let d = seal_n(&mut cs, 3);
    assert!(so.open(&d[0]).unwrap().newest);
    assert!(so.open(&d[2]).unwrap().newest);
    let late = so.open(&d[1]).unwrap();
    assert_eq!((late.counter, late.newest), (1, false));
}

#[test]
fn replayed_and_too_old() {
    let (mut cs, _, _, mut so, _) = pair(1);
    let d = seal_n(&mut cs, REPLAY_WINDOW as usize + 1);
    assert!(so.open(&d[1]).is_ok());
    assert_eq!(so.open(&d[1]), Err(Refusal::Replayed));
    assert!(so.open(&d[REPLAY_WINDOW as usize]).is_ok());
    assert!(so.open(&d[1]).is_err());
    assert_eq!(so.open(&d[0]), Err(Refusal::TooOld));
    assert_eq!(
        so.open(&d[1]),
        Err(Refusal::Replayed),
        "seen and still inside"
    );
    assert_eq!(so.forged(), 0, "none of these reached the AEAD");
}

#[test]
fn every_single_bit_flip_is_refused_and_poisons_nothing() {
    let (mut cs, _, _, mut so, _) = pair(0xabcdef);
    let mut d = Vec::new();
    cs.seal(b"payload", &mut d).unwrap();
    let mut forged = 0;
    for byte in 0..d.len() {
        for bit in 0..8 {
            let mut x = d.clone();
            x[byte] ^= 1 << bit;
            match so.open(&x) {
                Err(Refusal::Forged) => forged += 1,
                Err(Refusal::Malformed) => assert!(byte == 0 || byte == HEADER_LEN_C2S - 1),
                other => panic!("byte {byte} bit {bit}: {other:?}"),
            }
        }
    }
    assert_eq!(so.forged(), forged);
    assert_eq!(so.generation(), 0, "a forged phase flip promotes nothing");
    assert_eq!(
        so.open(&d).unwrap().plaintext,
        b"payload",
        "the genuine one still opens"
    );
}

#[test]
fn malformed_datagrams() {
    let (mut cs, _, _, mut so, _) = pair(1);
    let mut d = Vec::new();
    cs.seal(b"", &mut d).unwrap();
    assert_eq!(so.open(&d[..d.len() - 1]), Err(Refusal::Malformed), "short");
    assert_eq!(so.open(&[]), Err(Refusal::Malformed));
    let h = Header {
        phase: KeyPhase::Zero,
        cid: Some(1),
        counter: SEAL_LIMIT,
    };
    let (b, len) = h.encode();
    let over = [&b[..len], &[0; TAG_LEN]].concat();
    assert_eq!(
        so.open(&over),
        Err(Refusal::Malformed),
        "counter past the limit"
    );
    assert_eq!(so.forged(), 0);
}

#[test]
fn wrong_phase_below_the_top_is_refused_without_the_aead() {
    let (mut cs, _, _, mut so, _) = pair(1);
    let d = seal_n(&mut cs, 11);
    so.open(&d[0]).unwrap();
    so.open(&d[10]).unwrap();
    let mut flipped = d[5].clone();
    flipped[0] ^= KIND_PHASE_BIT;
    assert_eq!(so.open(&flipped), Err(Refusal::WrongPhase));
    assert_eq!(so.forged(), 0);
    assert!(so.open(&d[5]).is_ok());
}

#[test]
fn integrity_limit_closes_the_opener() {
    let (mut cs, _, _, mut so, _) = pair(1);
    let d = seal_n(&mut cs, 3);
    so.set_forged_for_test(INTEGRITY_LIMIT);
    assert!(so.open(&d[0]).is_ok(), "at the limit, not past it");
    let mut bad = d[1].clone();
    *bad.last_mut().unwrap() ^= 1;
    assert_eq!(so.open(&bad), Err(Refusal::Forged));
    assert_eq!(so.forged(), INTEGRITY_LIMIT + 1);
    assert_eq!(
        so.open(&d[1]),
        Err(Refusal::IntegrityLimit),
        "even a genuine one"
    );
    assert_eq!(so.open(&d[2]), Err(Refusal::IntegrityLimit));
}

#[test]
fn the_sealer_stops_at_the_counter_limit() {
    let (mut cs, _, _, mut so, _) = pair(1);
    cs.set_next_counter_for_test(SEAL_LIMIT - 1);
    let mut d = Vec::new();
    assert_eq!(cs.seal(b"last", &mut d), Ok(SEAL_LIMIT - 1));
    let mut more = Vec::new();
    assert_eq!(
        cs.seal(b"one more", &mut more),
        Err(SealError::CounterExhausted)
    );
    assert!(more.is_empty(), "nothing written on refusal");
    assert_eq!(cs.next_counter(), SEAL_LIMIT - 1 + 1);
    assert_eq!(so.open(&d).unwrap().counter, SEAL_LIMIT - 1);
}

#[test]
fn refusal_names_are_distinct() {
    let mut names: Vec<_> = Refusal::ALL.iter().map(|r| r.name()).collect();
    names.sort_unstable();
    names.dedup();
    assert_eq!(names.len(), Refusal::ALL.len());
}
