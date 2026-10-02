//! Key phases: the sealer's rekey rules and the opener following them
//! with reordered datagrams across the boundary.

use super::super::wire::KeyPhase;
use super::*;

const W: usize = REPLAY_WINDOW as usize;

/// Seals a full generation (W datagrams) and confirms it, ready to rekey.
fn ready(cs: &mut Sealer) -> Vec<Vec<u8>> {
    let d = seal_n(cs, W);
    cs.note_peer_ack(cs.next_counter() - 1);
    d
}

#[test]
fn rekey_needs_distance_and_a_confirmed_phase() {
    let (mut cs, _, _, _, _) = pair(1);
    seal_n(&mut cs, W - 1);
    cs.note_peer_ack(0);
    assert_eq!(cs.rekey(), Err(SealError::RekeyTooSoon));
    seal_n(&mut cs, 1);
    assert_eq!(cs.rekey(), Ok(()));
    assert_eq!((cs.generation(), cs.phase()), (1, KeyPhase::One));
    seal_n(&mut cs, W);
    cs.note_peer_ack(0); // an ack from the previous phase confirms nothing
    cs.note_peer_ack(cs.next_counter()); // nor one for a counter never sent
    assert_eq!(cs.rekey(), Err(SealError::RekeyUnconfirmed));
    cs.note_peer_ack(W as u64);
    assert_eq!(cs.rekey(), Ok(()));
    assert_eq!((cs.generation(), cs.phase()), (2, KeyPhase::Zero));
}

#[test]
fn reordered_datagrams_across_the_phase_boundary() {
    let (mut cs, _, _, mut so, _) = pair(1);
    let old = ready(&mut cs);
    cs.rekey().unwrap();
    let new = seal_n(&mut cs, 10);
    for d in &old[..W - 5] {
        so.open(d).unwrap();
    }
    // A new-phase datagram overtakes the end of the old phase.
    assert_eq!(so.open(&new[2]).unwrap().counter, W as u64 + 2);
    assert_eq!(so.generation(), 1);
    assert!(so.holds_previous_key());
    for d in &old[W - 5..] {
        assert!(so.open(d).is_ok(), "previous-phase grace");
    }
    for d in &new[..2] {
        assert!(so.open(d).is_ok(), "earlier new-phase datagrams");
    }
    assert_eq!(so.open(&new[2]), Err(Refusal::Replayed));
    assert_eq!(so.open(&old[W - 5]), Err(Refusal::Replayed));
    assert_eq!(so.open(&old[0]), Err(Refusal::TooOld));
    assert_eq!(so.forged(), 0);
}

#[test]
fn the_previous_key_goes_once_the_window_passed_the_boundary() {
    let (mut cs, _, _, mut so, _) = pair(1);
    let old = ready(&mut cs);
    cs.rekey().unwrap();
    let new = seal_n(&mut cs, W);
    so.open(&old[0]).unwrap();
    so.open(&new[0]).unwrap(); // promotes; cur_start = W
    for d in &new[1..W - 1] {
        so.open(d).unwrap();
    }
    assert!(
        so.holds_previous_key(),
        "top = 2W-2: counter W-1 is still inside"
    );
    assert!(so.open(&old[W - 1]).is_ok());
    so.open(&new[W - 1]).unwrap();
    assert!(
        !so.holds_previous_key(),
        "top = 2W-1: every old counter is too old"
    );
    assert_eq!(so.open(&old[W - 2]), Err(Refusal::TooOld));
}

#[test]
fn several_generations_in_a_row() {
    let (mut cs, _, _, mut so, _) = pair(1);
    for generation in 1..=4u64 {
        let d = ready(&mut cs);
        for x in &d {
            so.open(x).unwrap();
        }
        cs.rekey().unwrap();
        assert_eq!(so.generation(), generation - 1);
    }
    let mut d = Vec::new();
    cs.seal(b"gen 4", &mut d).unwrap();
    assert_eq!(so.open(&d).unwrap().plaintext, b"gen 4");
    assert_eq!(so.generation(), 4);
}

#[test]
fn server_to_client_rekeys_the_same_way() {
    let (_, mut co, mut ss, _, _) = pair(1);
    let old = ready(&mut ss);
    ss.rekey().unwrap();
    let new = seal_n(&mut ss, 2);
    assert!(co.open(&new[1]).is_ok());
    assert!(co.open(&old[W - 1]).is_ok());
    assert!(co.open(&new[0]).is_ok());
    assert_eq!(co.generation(), 1);
}
