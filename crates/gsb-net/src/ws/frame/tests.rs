//! Frame encoding: server frames are never masked, and one binary
//! message carries exactly one length-prefixed game frame.

use super::*;
use gsb_protocol::FrameBody;
use crate::ws::{OP_BIN, OP_PING};

#[test]
fn server_frames_are_unmasked_with_correct_lengths() {
    let small = encode_server_frame(OP_BIN, b"ab");
    assert_eq!(small, vec![0x82, 0x02, b'a', b'b']);

    let mid_payload = vec![7u8; 300]; // forces the 16-bit length form
    let mid = encode_server_frame(OP_PING, &mid_payload);
    assert_eq!(&mid[..4], &[0x89, 126, 0x01, 0x2c]);

    let big_payload = vec![9u8; 70_000]; // forces the 64-bit length form
    let big = encode_server_frame(OP_BIN, &big_payload);
    assert_eq!(&big[..2], &[0x82, 127]);
    assert_eq!(&big[2..10], &(70_000u64).to_be_bytes());

    for frame in [&small, &mid, &big] {
        // Mask bit clear on every server frame.
        assert_eq!(frame[1] & 0x80, 0, "server frames must not be masked");
    }
}

#[test]
fn game_envelope_roundtrips_like_tcp_framing() {
    let frame = FrameBody::new(0x1234, vec![1, 2, 3]);
    let env = encode_game_envelope(&frame);
    // Same prefix layout as framed.rs: LE length covering op+payload.
    assert_eq!(&env[..4], &((2 + 3usize) as u32).to_le_bytes());
    let parsed = FrameBody::decode(env.slice(4..)).unwrap();
    assert_eq!(parsed.op, 0x1234);
    assert_eq!(parsed.payload.as_ref(), &[1, 2, 3]);
}
