//! RFC 6455 §5.4 at the reader: a fragmented message may be interrupted
//! by CONTROL frames only. Any data frame that starts a new message while
//! one is still open is a protocol error (1002) — it must neither discard
//! the half-built message nor be delivered inside it.

use super::rig::*;
use super::*;

/// The game frame every test here fragments, and its envelope split in
/// two halves (first frame / final continuation).
fn halves() -> (Vec<u8>, Vec<u8>) {
    let env = encode_game_envelope(&FrameBody::new(9, b"fragmented".as_slice()));
    let (a, b) = env.split_at(env.len() / 2);
    (a.to_vec(), b.to_vec())
}

fn other_envelope() -> Vec<u8> {
    encode_game_envelope(&FrameBody::new(5, b"intruder".as_slice())).to_vec()
}

#[tokio::test]
async fn a_new_unfinished_binary_frame_inside_an_open_message_fails_with_1002() {
    let mut rig = ReaderRig::new().await;
    let (first, _) = halves();
    rig.send(false, OP_BIN, &first).await;
    // Before the guard this silently threw the first half away and
    // started over with the intruder.
    rig.send(false, OP_BIN, &other_envelope()).await;
    assert_eq!(rig.failure_code().await, 1002);
}

#[tokio::test]
async fn a_new_complete_binary_frame_inside_an_open_message_fails_with_1002() {
    let mut rig = ReaderRig::new().await;
    let (first, _) = halves();
    rig.send(false, OP_BIN, &first).await;
    // Before the guard this was delivered as a game frame INSIDE the
    // open message.
    rig.send(true, OP_BIN, &other_envelope()).await;
    assert_eq!(rig.failure_code().await, 1002);
}

/// Text alone is 1003 (the contract has no text); text inside an open
/// binary message is first of all a framing violation, so 1002 wins —
/// the same code a text-capable endpoint must send (Autobahn 5.18 pins
/// 1002 for text-inside-text).
#[tokio::test]
async fn a_text_frame_inside_an_open_binary_message_fails_with_1002() {
    let mut rig = ReaderRig::new().await;
    let (first, _) = halves();
    rig.send(false, OP_BIN, &first).await;
    rig.send(true, OP_TEXT, b"hi").await;
    assert_eq!(rig.failure_code().await, 1002);
}

#[tokio::test]
async fn a_text_frame_with_no_message_open_still_fails_with_1003() {
    let mut rig = ReaderRig::new().await;
    rig.send(false, OP_TEXT, b"hi").await;
    assert_eq!(rig.failure_code().await, 1003);
}

#[tokio::test]
async fn a_continuation_with_no_message_open_fails_with_1002() {
    let mut rig = ReaderRig::new().await;
    rig.send(true, OP_CONT, &other_envelope()).await;
    assert_eq!(rig.failure_code().await, 1002);
}

/// Ping and pong between the fragments: the pong is answered with the
/// ping's data, the unsolicited pong is ignored, and the message still
/// reassembles into the one game frame — then the reader accepts a fresh
/// message (the fragment state was reset, not poisoned).
#[tokio::test]
async fn ping_and_pong_between_fragments_leave_the_message_intact() {
    let mut rig = ReaderRig::new().await;
    let (first, last) = halves();
    rig.send(false, OP_BIN, &first).await;
    rig.send(true, OP_PING, b"mid").await;
    rig.send(true, OP_PONG, b"unasked").await;
    rig.send(true, OP_CONT, &last).await;

    let frame = rig.game().await;
    assert_eq!(
        (frame.op, frame.payload.as_ref()),
        (9, b"fragmented".as_slice())
    );
    assert_eq!(rig.drain(), vec![Queued::Control(OP_PONG, b"mid".to_vec())]);

    rig.send(true, OP_BIN, &other_envelope()).await;
    assert_eq!(rig.game().await.op, 5);
}

/// A close between the fragments is legal: the handshake completes (code
/// echoed, socket shut) and the half-built message is simply abandoned —
/// no 1002, no delivery.
#[tokio::test]
async fn a_close_between_fragments_completes_the_close_handshake() {
    let mut rig = ReaderRig::new().await;
    let (first, _) = halves();
    rig.send(false, OP_BIN, &first).await;
    rig.send(true, OP_CLOSE, &1001u16.to_be_bytes()).await;

    assert!(rig.next().await.is_none(), "close ends the stream");
    assert_eq!(
        rig.drain(),
        vec![
            Queued::Control(OP_CLOSE, 1001u16.to_be_bytes().to_vec()),
            Queued::Shutdown,
        ]
    );
}
