//! RFC 6455 frame primitives: masking, the server-side frame encoder
//! (server frames are never masked), and the game envelope that rides
//! inside one binary message.

use bytes::Bytes;
use bytes::BytesMut;

use gsb_protocol::FrameBody;

/// XOR `payload` in place with the 4-byte mask (used both directions in the
/// tests; on the server read path it unmasks client frames).
pub(super) fn apply_mask(payload: &mut [u8], key: [u8; 4]) {
    for (i, b) in payload.iter_mut().enumerate() {
        *b ^= key[i & 3];
    }
}

/// Encode ONE complete server-to-client frame: FIN set, never masked
/// (RFC 6455 §5.1: a server MUST NOT mask). Lengths ≥ 126/65536 use the
/// 16-bit / 64-bit extended forms (network byte order).
pub(super) fn encode_server_frame(opcode: u8, payload: &[u8]) -> Vec<u8> {
    let mut frame = Vec::with_capacity(10 + payload.len());
    frame.push(0x80 | opcode);
    if payload.len() < 126 {
        frame.push(payload.len() as u8);
    } else if payload.len() <= u16::MAX as usize {
        frame.push(126);
        frame.extend_from_slice(&(payload.len() as u16).to_be_bytes());
    } else {
        frame.push(127);
        frame.extend_from_slice(&(payload.len() as u64).to_be_bytes());
    }
    frame.extend_from_slice(payload);
    frame
}

/// Wrap a game frame into its WS-message body:
/// `[u32 LE len][body]` where body = `[u16 LE op][payload]` — the exact
/// envelope `crate::framed` puts on raw TCP (see the module docs).
pub(super) fn encode_game_envelope(frame: &FrameBody) -> Bytes {
    let body = frame.encode();
    let mut out = BytesMut::with_capacity(4 + body.len());
    out.extend_from_slice(&(body.len() as u32).to_le_bytes());
    out.extend_from_slice(&body);
    out.freeze()
}

/// A fully received (already unmasked) WS frame from the client.
pub(super) struct RawFrame {
    pub(super) fin: bool,
    pub(super) opcode: u8,
    pub(super) payload: Vec<u8>,
}

#[cfg(test)]
mod tests;
