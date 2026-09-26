//! Byte pins: what this crate puts on the wire is exactly what the
//! hand-rolled client copies it replaced put there.
//!
//! The copies are frozen here verbatim (they were deleted with the
//! migration): the four spellings of the frame encoder that lived in the
//! load generator, the example client and the server suites. Every one
//! must agree with [`encode`] byte for byte, over empty, short and
//! multi-kilobyte payloads and both opcode bands; the base frames are
//! pinned as literal bytes on top.

use crate::frame::encode;
use crate::session::*;

/// `gsb-loadgen`'s `wire::frame` — also the example client's `frame`,
/// `tls_e2e`'s `frame`, and `e2e` / `multi_listener`'s `framed`.
fn loadgen_frame(op: u16, payload: &[u8]) -> Vec<u8> {
    let body = 2 + payload.len();
    let mut out = Vec::with_capacity(4 + body);
    out.extend_from_slice(&(body as u32).to_le_bytes());
    out.extend_from_slice(&op.to_le_bytes());
    out.extend_from_slice(payload);
    out
}

/// The hosted suites' `Client::send`, `economy_rooms` / `game_module`'s
/// `write_frame`.
fn hosted_frame(op: u16, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(6 + payload.len());
    out.extend_from_slice(&((2 + payload.len()) as u32).to_le_bytes());
    out.extend_from_slice(&op.to_le_bytes());
    out.extend_from_slice(payload);
    out
}

/// `server_stop` / `server_closes` / the close-notice client's `frame`.
fn stop_frame(op: u16, payload: &[u8]) -> Vec<u8> {
    let mut out = ((2 + payload.len()) as u32).to_le_bytes().to_vec();
    out.extend_from_slice(&op.to_le_bytes());
    out.extend_from_slice(payload);
    out
}

/// `write_stall`'s `send` (body first, then the prefix).
fn stall_frame(op: u16, payload: &[u8]) -> Vec<u8> {
    let mut body = Vec::with_capacity(2 + payload.len());
    body.extend_from_slice(&op.to_le_bytes());
    body.extend_from_slice(payload);
    let mut out = (body.len() as u32).to_le_bytes().to_vec();
    out.extend_from_slice(&body);
    out
}

#[test]
fn encode_matches_every_replaced_encoder() {
    let payloads: Vec<Vec<u8>> = vec![
        vec![],
        vec![0x08, 0x01],
        (0..=255u8).collect(),
        vec![0xab; 70_000],
    ];
    for op in [1u16, 3, 9, 12, 1000, 1003, u16::MAX] {
        for p in &payloads {
            let want = encode(op, p);
            assert_eq!(
                loadgen_frame(op, p),
                want,
                "loadgen op {op} len {}",
                p.len()
            );
            assert_eq!(hosted_frame(op, p), want, "hosted op {op} len {}", p.len());
            assert_eq!(stop_frame(op, p), want, "stop op {op} len {}", p.len());
            assert_eq!(stall_frame(op, p), want, "stall op {op} len {}", p.len());
        }
    }
}

/// AUTH as every reference client built it: the name, no ticket, the
/// protocol version.
#[test]
fn auth_frame_bytes_are_pinned() {
    let f = auth_req(&Credentials::named("bot-1"));
    let wire = encode(f.op, &f.payload);
    #[rustfmt::skip]
    let want = [
        0x0b, 0, 0, 0, 0x01, 0x00,             // len 11, AUTH_REQ
        0x0a, 0x05, b'b', b'o', b't', b'-', b'1', // name
        0x18, 0x01,                            // protocol_version 1
    ];
    assert_eq!(wire, want);
    let old = gsb_protocol::base::Auth {
        name: "bot-1".into(),
        ticket: vec![],
        protocol_version: gsb_protocol::PROTOCOL_VERSION,
    };
    assert_eq!(wire, loadgen_frame(1, &prost::Message::encode_to_vec(&old)));
}

/// AUTH with a ticket (the hosted suites' `join_with_ticket`).
#[test]
fn ticket_auth_frame_bytes_are_pinned() {
    let f = auth_req(&Credentials::named("n").with_ticket(b"tk".to_vec()));
    #[rustfmt::skip]
    let want = [
        0x0b, 0, 0, 0, 0x01, 0x00,             // len 11, AUTH_REQ
        0x0a, 0x01, b'n',          // name
        0x12, 0x02, b't', b'k',    // ticket
        0x18, 0x01,                // protocol_version
    ];
    assert_eq!(encode(f.op, &f.payload), want);
}

#[test]
fn join_leave_heartbeat_frame_bytes_are_pinned() {
    let wire = |f: gsb_protocol::FrameBody| encode(f.op, &f.payload);
    assert_eq!(wire(join_req(1)), [4, 0, 0, 0, 3, 0, 0x08, 0x01]);
    assert_eq!(wire(join_req(0)), [2, 0, 0, 0, 3, 0]);
    assert_eq!(wire(leave_req()), [2, 0, 0, 0, 5, 0]);
    assert_eq!(wire(heartbeat(0)), [2, 0, 0, 0, 7, 0]);
    assert_eq!(wire(heartbeat(7)), [4, 0, 0, 0, 7, 0, 0x08, 0x07]);
}

/// The pipelined hello is the load generator's coalesced AUTH + JOIN
/// write, byte for byte.
#[tokio::test]
async fn hello_is_the_coalesced_auth_and_join() {
    let (a, b) = tokio::io::duplex(1 << 12);
    let mut conn = crate::Conn::stream(a);
    hello(&mut conn, &Credentials::named("bot-7"), 3)
        .await
        .unwrap();
    drop(conn);
    let mut got = Vec::new();
    tokio::io::AsyncReadExt::read_to_end(&mut { b }, &mut got)
        .await
        .unwrap();
    let auth = gsb_protocol::base::Auth {
        name: "bot-7".into(),
        ticket: vec![],
        protocol_version: gsb_protocol::PROTOCOL_VERSION,
    };
    let mut want = loadgen_frame(1, &prost::Message::encode_to_vec(&auth));
    let join = gsb_protocol::base::JoinRoom { room_id: 3 };
    want.extend(loadgen_frame(3, &prost::Message::encode_to_vec(&join)));
    assert_eq!(got, want);
}
