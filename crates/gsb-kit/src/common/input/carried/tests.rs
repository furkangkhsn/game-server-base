//! The carried slot: a dropped frame's ack and session payload are owed
//! again, and nothing else is.

use bytes::BytesMut;
use gsb_core::id::PlayerId;
use prost::Message;

use crate::common::{InputSeq, emit_private};
use crate::proto::{Private, private::Payload};

/// Write `player`'s ordinary frame (ack/responses only) the way the
/// session writer does; `Some(ack)` when it carried one.
fn frame(input: &mut InputSeq, player: PlayerId) -> Option<u64> {
    input.frame(player);
    let mut out = BytesMut::new();
    if !emit_private(input, player, &[], &mut out) {
        return None;
    }
    match Private::decode(out.as_ref()).expect("a frame").payload {
        Some(Payload::Ack(ack)) => Some(ack.processed_up_to),
        _ => None,
    }
}

/// An ack whose batch was dropped is sent again — the mark as it is
/// then — and only once more.
#[test]
fn a_dropped_ack_is_sent_again() {
    let mut input = InputSeq::default();
    let p = PlayerId(1);
    input.begin(p);
    assert!(input.admit(p, 3));
    assert_eq!(frame(&mut input, p), Some(3));
    assert!(!input.dropped(p), "not a one-shot full");
    assert_eq!(frame(&mut input, p), Some(3), "owed again");
    assert_eq!(frame(&mut input, p), None, "delivered: nothing more");

    // Dropped, and the mark moved on before the next frame: the next
    // frame reports the new mark.
    assert!(input.admit(p, 5));
    assert_eq!(frame(&mut input, p), Some(5));
    input.dropped(p);
    assert!(input.admit(p, 6));
    assert_eq!(frame(&mut input, p), Some(6));
    assert_eq!(frame(&mut input, p), None);
}

/// A dropped frame that carried no ack re-arms none; a drop reported for
/// another player than the frame's re-arms nothing (the core reports a
/// drop right after THAT player's frame).
#[test]
fn only_what_rode_the_dropped_frame_is_rearmed() {
    let mut input = InputSeq::default();
    let (p, q) = (PlayerId(1), PlayerId(2));
    input.begin(p);
    input.begin(q);
    assert!(input.admit(p, 1));
    assert_eq!(frame(&mut input, p), Some(1));
    assert_eq!(frame(&mut input, p), None);
    input.dropped(p);
    assert_eq!(frame(&mut input, p), None, "the dropped frame had no ack");

    assert!(input.admit(p, 2));
    assert_eq!(frame(&mut input, p), Some(2));
    assert_eq!(frame(&mut input, q), None);
    input.dropped(p);
    assert_eq!(frame(&mut input, p), None, "the slot was q's frame");
}

/// The session payload: owed again after its frame's drop; a one-shot
/// full is reported back to the room; a drop consumes the slot.
#[test]
fn a_dropped_greeting_is_owed_again_and_a_full_is_reported() {
    let mut input = InputSeq::default();
    let p = PlayerId(1);
    input.begin(p);
    input.frame(p);
    assert!(input.take_greeting(p));
    input.carries_greeting();
    input.carries_full();
    assert!(input.dropped(p), "the frame was a one-shot full");
    assert!(
        !input.dropped(p),
        "the slot is consumed by the first report"
    );

    input.frame(p);
    assert!(input.take_greeting(p), "owed again");
    input.carries_greeting();
    input.frame(p);
    assert!(!input.take_greeting(p), "delivered: never again");
    input.dropped(p);
    input.frame(p);
    assert!(!input.take_greeting(p), "that frame carried no greeting");

    // The migration carry sees the owed greeting.
    input.frame(p);
    input.carries_greeting();
    input.dropped(p);
    assert_eq!(input.mark(p), Some((0, 0, true)));
}

/// A player that left between the drop and its next frame: nothing is
/// resurrected.
#[test]
fn a_drop_after_the_leave_resurrects_nothing() {
    let mut input = InputSeq::default();
    let p = PlayerId(1);
    input.begin(p);
    assert!(input.admit(p, 4));
    assert_eq!(frame(&mut input, p), Some(4));
    input.dropped(p);
    input.end(p);
    assert_eq!(input.mark(p), None, "the leave ends the re-armed session");
    input.dropped(p);
    assert_eq!(input.mark(p), None);
}
