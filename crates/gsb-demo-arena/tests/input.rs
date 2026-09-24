//! Input sequence / acknowledgment through the real room actor: the rule
//! is the kit's (`InputSeq`), the decoding the arena's. Numbered inputs
//! advance a monotonic high-water mark reported in the `Private` frame's
//! `InputAck`; a duplicate or reordered-late input is dropped silently
//! (it never regresses the unit to a stale target); an unnumbered one
//! (seq 0) is applied but never acknowledged.

mod common;

use common::{Arena, SETTLE};
use gsb_demo_arena::ArenaGame;
use gsb_demo_arena::codec::Cm3;

fn cm(x: i32, y: i32, z: i32) -> Cm3 {
    Cm3 { x, y, z }
}

#[tokio::test]
async fn numbered_inputs_are_acked_and_stale_ones_dropped() {
    let mut arena = Arena::new(ArenaGame::default());
    let mut cs = vec![arena.join(1).await];
    let me = cs[0].id;
    let at = |cs: &[common::Client]| cs[0].view[&me];

    // seq 1: acknowledged, applied.
    cs[0].move_to(0.0, 10.0, 0.0, 1).await;
    arena.advance(&mut cs, SETTLE).await;
    assert_eq!(cs[0].acks, [1], "seq 1 acknowledged once");
    assert_eq!(at(&cs), cm(0, 1000, 0));

    // seq 3 (2 lost on the way): the mark jumps — high-water, not
    // contiguity.
    cs[0].move_to(5.0, 5.0, 5.0, 3).await;
    arena.advance(&mut cs, SETTLE).await;
    assert_eq!(cs[0].acks, [1, 3]);
    assert_eq!(at(&cs), cm(500, 500, 500));

    // The late seq 2 and a duplicate seq 3 arrive: both dropped — no
    // ack, and the unit does not regress toward either stale target.
    cs[0].move_to(-30.0, 0.0, -30.0, 2).await;
    cs[0].move_to(30.0, 20.0, 30.0, 3).await;
    arena.advance(&mut cs, SETTLE).await;
    assert_eq!(cs[0].acks, [1, 3], "stale inputs are never acknowledged");
    assert_eq!(at(&cs), cm(500, 500, 500), "not applied");

    // Unnumbered (seq 0): applied, the mark stays.
    cs[0].move_to(-5.0, 0.0, -5.0, 0).await;
    arena.advance(&mut cs, SETTLE).await;
    assert_eq!(at(&cs), cm(-500, 0, -500));
    assert_eq!(cs[0].acks, [1, 3], "seq 0 is never acknowledged");

    // The next numbered input continues from the mark.
    cs[0].move_to(1.23, 5.68, -10.0, 4).await;
    arena.advance(&mut cs, SETTLE).await;
    assert_eq!(cs[0].acks, [1, 3, 4]);
    assert_eq!(at(&cs), cm(123, 568, -1000));
}
