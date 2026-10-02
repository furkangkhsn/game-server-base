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

/// Encode the accept (or a re-answered proof): `ACK{next}`, and the
/// session's CID after it when the server granted one (module `path`;
/// an older client reads bytes 1..5 and ignores the rest).
pub(super) fn encode_accept(next: u32, cid: Option<u64>) -> Vec<u8> {
    let mut d = encode_ack(next);
    if let Some(cid) = cid {
        d.extend_from_slice(&cid.to_le_bytes());
    }
    d
}

/// Encode a proof: the HELLO, and the client's capability byte after it
/// when it has one to ask for (module `path`; an older server reads
/// bytes 1..17 and ignores the rest).
pub(super) fn encode_proof(nonce: u64, cookie: u64, caps: u8) -> Vec<u8> {
    let mut d = encode_hello(nonce, cookie);
    if caps != 0 {
        d.push(caps);
    }
    d
}

/// Encode a PATH_CHALLENGE (server → client; module `path`).
pub(super) fn encode_path_challenge(nonce: u64) -> Vec<u8> {
    let mut d = Vec::with_capacity(9);
    d.push(KIND_PATH_CHALLENGE);
    d.extend_from_slice(&nonce.to_le_bytes());
    d
}

/// Tag a client → server datagram `d` with the session's CID: `[kind |
/// 0x80][u64 LE cid][the rest of d]` (module `path`).
pub(super) fn tag(cid: u64, d: &[u8]) -> Vec<u8> {
    let mut t = Vec::with_capacity(d.len() + 8);
    t.push(d[0] | KIND_CID_TAG);
    t.extend_from_slice(&cid.to_le_bytes());
    t.extend_from_slice(&d[1..]);
    t
}

/// Encode a PATH_RESPONSE (client → server, always tagged).
pub(super) fn encode_path_response(cid: u64, nonce: u64) -> Vec<u8> {
    let mut d = Vec::with_capacity(17);
    d.push(KIND_PATH_RESPONSE | KIND_CID_TAG);
    d.extend_from_slice(&cid.to_le_bytes());
    d.extend_from_slice(&nonce.to_le_bytes());
    d
}

/// A little-endian `u64` at `at` in `d`, if `d` is long enough.
pub(super) fn u64_at(d: &[u8], at: usize) -> Option<u64> {
    Some(u64::from_le_bytes(d.get(at..at + 8)?.try_into().ok()?))
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
