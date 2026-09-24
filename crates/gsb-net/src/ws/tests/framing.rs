//! RFC 6455 §5.1-§5.2 / §5.5 at the reader: the frame-header rules the
//! Autobahn fuzzing client probes (reserved bits, reserved opcodes,
//! control-frame limits, the client mask, the payload-length encoding).

use super::rig::*;
use super::*;

/// A masked frame with a hand-written first byte and length field
/// (`len[0]` is the 7-bit length WITHOUT the mask bit; the rest is the
/// extended length as it should appear on the wire).
fn raw_frame(b0: u8, len: &[u8], payload: &[u8]) -> Vec<u8> {
    let key = [0x11, 0x22, 0x33, 0x44];
    let mut frame = vec![b0, 0x80 | len[0]];
    frame.extend_from_slice(&len[1..]);
    frame.extend_from_slice(&key);
    frame.extend(payload.iter().enumerate().map(|(i, b)| b ^ key[i & 3]));
    frame
}

/// A valid one-frame game envelope whose WS payload is `n` bytes long.
fn envelope_of(n: usize) -> Vec<u8> {
    encode_game_envelope(&FrameBody::new(3, vec![0xab; n - 6])).to_vec()
}

#[tokio::test]
async fn reserved_bits_without_an_extension_fail_with_1002() {
    for rsv in [0x40, 0x20, 0x10, 0x70] {
        let mut rig = ReaderRig::new().await;
        rig.send_raw(&raw_frame(0x80 | rsv | OP_BIN, &[8], &envelope_of(8)))
            .await;
        assert_eq!(rig.failure_code().await, 1002, "rsv {rsv:#04x}");
    }
}

#[tokio::test]
async fn reserved_opcodes_fail_with_1002() {
    for op in (0x3..=0x7).chain(0xB..=0xF) {
        let mut rig = ReaderRig::new().await;
        rig.send(true, op, b"x").await;
        assert_eq!(rig.failure_code().await, 1002, "opcode {op:#x}");
    }
}

#[tokio::test]
async fn oversized_or_fragmented_control_frames_fail_with_1002() {
    let cases: [(bool, u8, usize); 4] = [
        (true, OP_PING, 126),
        (true, OP_CLOSE, 126),
        (false, OP_PING, 2),
        (false, OP_PONG, 2),
    ];
    for (fin, op, len) in cases {
        let mut rig = ReaderRig::new().await;
        let mut payload = vec![b'a'; len];
        if op == OP_CLOSE {
            payload[..2].copy_from_slice(&1000u16.to_be_bytes());
        }
        rig.send(fin, op, &payload).await;
        assert_eq!(rig.failure_code().await, 1002, "fin {fin} op {op:#x}");
    }
    // The boundary itself is legal: a 125-byte ping is answered.
    let mut rig = ReaderRig::new().await;
    rig.send(true, OP_PING, &[b'p'; 125]).await;
    rig.send(true, OP_BIN, &envelope_of(8)).await;
    assert_eq!(rig.game().await.op, 3);
    assert_eq!(rig.drain(), vec![Queued::Control(OP_PONG, vec![b'p'; 125])]);
}

#[tokio::test]
async fn an_unmasked_client_frame_fails_with_1002() {
    let mut rig = ReaderRig::new().await;
    let env = envelope_of(8);
    rig.send_raw(&encode_client_frame(true, OP_BIN, &env, [0; 4], false))
        .await;
    assert_eq!(rig.failure_code().await, 1002);
}

/// §5.2: "the most significant bit MUST be 0". A set MSB is a malformed
/// header, not merely a large message: 1002, not the ceiling's 1009.
#[tokio::test]
async fn a_64_bit_length_with_the_high_bit_set_fails_with_1002() {
    let mut rig = ReaderRig::new().await;
    let mut len = vec![127];
    len.extend_from_slice(&(0x8000_0000_0000_0008u64).to_be_bytes());
    rig.send_raw(&raw_frame(0x80 | OP_BIN, &len, &envelope_of(8)))
        .await;
    assert_eq!(rig.failure_code().await, 1002);
}

/// §5.2: "the minimal number of bytes MUST be used to encode the length".
/// A browser never breaks it; accepting it would let two encodings of one
/// frame disagree with an intermediary's parser.
#[tokio::test]
async fn non_minimal_length_encodings_fail_with_1002() {
    let env = envelope_of(8);
    let mut long16 = vec![126];
    long16.extend_from_slice(&(env.len() as u16).to_be_bytes());
    let mut long64 = vec![127];
    long64.extend_from_slice(&(env.len() as u64).to_be_bytes());
    let mut mid64 = vec![127];
    let mid = envelope_of(65_535); // the largest 16-bit length, in 64-bit form
    mid64.extend_from_slice(&(mid.len() as u64).to_be_bytes());
    for (len, payload) in [(long16, &env), (long64, &env), (mid64, &mid)] {
        let mut rig = ReaderRig::new().await;
        rig.send_raw(&raw_frame(0x80 | OP_BIN, &len, payload)).await;
        assert_eq!(rig.failure_code().await, 1002, "length field {len:?}");
    }
}

/// Every length at a form boundary, in its minimal form, is delivered.
#[tokio::test]
async fn minimal_lengths_at_every_form_boundary_are_delivered() {
    let mut rig = ReaderRig::new().await;
    for n in [6, 125, 126, 65_535, 65_536] {
        let env = envelope_of(n);
        assert_eq!(env.len(), n);
        rig.send(true, OP_BIN, &env).await;
        assert_eq!(rig.game().await.payload.len(), n - 6, "{n}-byte payload");
    }
}

/// The 64-bit form over the ceiling is still the 1009 size rejection.
#[tokio::test]
async fn a_64_bit_length_over_the_ceiling_still_fails_with_1009() {
    let mut rig = ReaderRig::with_max(64 * 1024).await;
    let mut len = vec![127];
    len.extend_from_slice(&(70_000u64).to_be_bytes());
    let mut frame = raw_frame(0x80 | OP_BIN, &len, &[]);
    frame.extend_from_slice(&[0; 16]); // a few payload bytes; never read
    rig.send_raw(&frame).await;
    assert_eq!(rig.failure_code().await, 1009);
}
