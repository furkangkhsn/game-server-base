//! Datagram encoding and parsing: the on-the-wire shapes of the rUDP
//! datagram kinds, shared by the server and client sides.

use bytes::Bytes;
use gsb_protocol::FrameBody;

use crate::udp::*;

/// Encode a RAW (lossy game-band) datagram.
pub(super) fn encode_raw(frame: &FrameBody) -> Vec<u8> {
    let body = frame.encode();
    let mut d = Vec::with_capacity(1 + body.len());
    d.push(KIND_RAW);
    d.extend_from_slice(&body);
    d
}

/// Encode a REL (reliable control-band) datagram.
pub(super) fn encode_rel(seq: u32, frame: &FrameBody) -> Vec<u8> {
    let body = frame.encode();
    let mut d = Vec::with_capacity(5 + body.len());
    d.push(KIND_REL);
    d.extend_from_slice(&seq.to_le_bytes());
    d.extend_from_slice(&body);
    d
}

/// Encode an ACK datagram (cumulative: the next expected seq).
pub(super) fn encode_ack(next: u32) -> Vec<u8> {
    let mut d = [0u8; 5];
    d[0] = KIND_ACK;
    d[1..5].copy_from_slice(&next.to_le_bytes());
    d.to_vec()
}

/// Encode a HELLO datagram.
pub(super) fn encode_hello(nonce: u64, cookie: u64) -> Vec<u8> {
    let mut d = [0u8; 18];
    d[0] = KIND_HELLO;
    d[1..9].copy_from_slice(&nonce.to_le_bytes());
    d[9..17].copy_from_slice(&cookie.to_le_bytes());
    d.to_vec()
}

/// Encode a PROBE datagram (server → client; module `feedback`): the
/// probe's id and the server's newest RTT sample in microseconds (0 =
/// none since the previous probe).
pub(super) fn encode_probe(id: u32, echo_us: u32) -> Vec<u8> {
    two_u32(KIND_PROBE, id, echo_us)
}

/// Encode a REPORT datagram (client → server; module `feedback`): the
/// probe it answers (0 = an announcement, no probe yet) and the game-band
/// datagrams received since the session began (wrapping).
pub(super) fn encode_report(id: u32, received: u32) -> Vec<u8> {
    two_u32(KIND_REPORT, id, received)
}

fn two_u32(kind: u8, a: u32, b: u32) -> Vec<u8> {
    let mut d = [0u8; 9];
    d[0] = kind;
    d[1..5].copy_from_slice(&a.to_le_bytes());
    d[5..9].copy_from_slice(&b.to_le_bytes());
    d.to_vec()
}

/// The two `u32` fields of a PROBE or REPORT body (the bytes after the
/// kind byte). Trailing bytes are ignored: room for additive fields.
pub(super) fn parse_two_u32(body: &[u8]) -> Option<(u32, u32)> {
    let a = u32::from_le_bytes(body.get(0..4)?.try_into().ok()?);
    let b = u32::from_le_bytes(body.get(4..8)?.try_into().ok()?);
    Some((a, b))
}

/// Parse the frame body out of a RAW/REL datagram (2-byte op + payload).
pub(super) fn body_of(d: &[u8], header: usize) -> Option<FrameBody> {
    let body = &d[header..];
    if body.len() < 2 {
        return None;
    }
    let op = u16::from_le_bytes([body[0], body[1]]);
    Some(FrameBody::new(op, Bytes::copy_from_slice(&body[2..])))
}
