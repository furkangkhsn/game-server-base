//! The protocol version gate (`docs/DESIGN.md` §5.5): the ONE version
//! check in the protocol, on the AUTH frame.
//!
//! Three branches, one per test: the server's own version is accepted,
//! `0` (a client built before the field existed) is accepted as legacy,
//! and anything else is refused with `ERROR_CODE_PROTOCOL_VERSION` —
//! without closing the connection and without spending the violation
//! budget.
//!
//! The actor is driven directly over its inbox, like `violation.rs`: a
//! registry mailbox with no receiver models "registry gone", and the out
//! channel is the actor's sole outbound, so "the channel closed" is the
//! observable teardown.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use gsb_core::channel::{FrameBatch, channel};
use gsb_core::conn::{ConnIn, ConnectionActor};
use gsb_core::id::ConnectionId;
use gsb_core::metrics::MetricsEvent;
use gsb_core::registry::RegistryMsg;
use gsb_protocol::base::{Auth, ErrorCode, Heartbeat};
use gsb_protocol::{PROTOCOL_VERSION, base, base_table, op};
use prost::Message;
use tokio::sync::mpsc;

const WAIT: Duration = Duration::from_secs(5);

fn spawn_actor(conn: u64) -> (mpsc::Sender<ConnIn>, mpsc::Receiver<FrameBatch>) {
    let (inbox_tx, inbox) = channel::<ConnIn>(128);
    let (out_tx, out_rx) = channel::<FrameBatch>(16);
    let (reg_tx, _reg_rx) = channel::<RegistryMsg>(16);
    let (metrics_tx, _metrics_rx) = mpsc::channel::<MetricsEvent>(16);
    let actor = ConnectionActor::new(
        ConnectionId(conn),
        SocketAddr::from(([127, 0, 0, 1], 41_000u16 + conn as u16)),
        Arc::new(base_table()),
        reg_tx,
        inbox,
        out_tx,
        metrics_tx,
        None, // local auth: the version gate runs before either auth path
    );
    tokio::spawn(actor.run());
    (inbox_tx, out_rx)
}

async fn send_auth(tx: &mpsc::Sender<ConnIn>, version: u32) {
    let payload = Auth {
        name: "v".into(),
        ticket: vec![],
        protocol_version: version,
    }
    .encode_to_vec();
    tx.send(ConnIn::Frame(gsb_protocol::FrameBody::new(
        op::base::AUTH_REQ,
        payload,
    )))
    .await
    .expect("inbox");
}

/// The next outbound frame's opcode and payload.
async fn next_frame(out: &mut mpsc::Receiver<FrameBatch>) -> (u16, bytes::Bytes) {
    let batch = tokio::time::timeout(WAIT, out.recv())
        .await
        .expect("timed out waiting for a frame")
        .expect("out channel closed");
    let f = batch.into_iter().next().expect("empty batch");
    (f.op, f.payload)
}

/// A client that states the server's own version authenticates normally.
#[tokio::test]
async fn the_servers_own_version_is_accepted() {
    let (tx, mut out) = spawn_actor(1);
    send_auth(&tx, PROTOCOL_VERSION).await;

    let (op_code, payload) = next_frame(&mut out).await;
    assert_eq!(
        op_code,
        op::base::AUTH_RESULT,
        "a matching version must authenticate, not error"
    );
    let r = base::AuthResult::decode(payload.as_ref()).expect("AuthResult decode");
    assert!(r.ok, "auth rejected: {}", r.reason);
}

/// `0` is what a client built before the field existed sends (proto3
/// does not encode a default-valued scalar). Refusing it would break
/// every existing client on the day the field lands, and the field
/// exists to establish compatibility — so it is accepted as legacy.
#[tokio::test]
async fn an_unversioned_legacy_client_is_accepted() {
    let (tx, mut out) = spawn_actor(2);
    send_auth(&tx, 0).await;

    let (op_code, payload) = next_frame(&mut out).await;
    assert_eq!(
        op_code,
        op::base::AUTH_RESULT,
        "version 0 is the legacy path and must be accepted"
    );
    let r = base::AuthResult::decode(payload.as_ref()).expect("AuthResult decode");
    assert!(r.ok, "legacy auth rejected: {}", r.reason);
}

/// Any other version is refused with the dedicated code, and the
/// connection STAYS ALIVE: a mismatch is a normal rejection (the frame
/// was well-formed — the client is simply the wrong build), never a
/// protocol violation. Liveness is proved by the actor still answering
/// an ordinary HEARTBEAT afterwards.
#[tokio::test]
async fn a_mismatched_version_is_refused_without_closing() {
    let (tx, mut out) = spawn_actor(3);
    let bogus = PROTOCOL_VERSION + 41;
    send_auth(&tx, bogus).await;

    let (op_code, payload) = next_frame(&mut out).await;
    assert_eq!(op_code, op::base::ERROR, "a mismatch must be answered");
    let e = base::Error::decode(payload.as_ref()).expect("Error decode");
    assert_eq!(
        e.code(),
        ErrorCode::ProtocolVersion,
        "wrong class for a version mismatch: {e:?}"
    );
    // Both numbers, so the client knows what to upgrade to.
    assert!(
        e.message.contains(&bogus.to_string()) && e.message.contains(&PROTOCOL_VERSION.to_string()),
        "the message must name both versions: {}",
        e.message
    );

    // Still alive: an ordinary heartbeat is still answered. (A pre-auth
    // heartbeat is answered at most once a second; this is the first.)
    tx.send(ConnIn::Frame(gsb_protocol::FrameBody::new(
        op::base::HEARTBEAT,
        Heartbeat { tick: 7 }.encode_to_vec(),
    )))
    .await
    .expect("inbox");
    let (op_code, payload) = next_frame(&mut out).await;
    assert_eq!(
        op_code,
        op::base::HEARTBEAT_ACK,
        "the connection must survive a version rejection"
    );
    let ack = base::HeartbeatAck::decode(payload.as_ref()).expect("HeartbeatAck decode");
    assert_eq!(ack.tick, 7);
}

/// A rejected version leaves the state machine in `WaitingAuth`: the
/// same connection can still authenticate, which is what makes the
/// rejection "normal" rather than terminal. (It also shows the gate did
/// not consume the auth state on the way out.)
#[tokio::test]
async fn a_rejected_version_does_not_block_a_later_correct_auth() {
    let (tx, mut out) = spawn_actor(4);
    send_auth(&tx, PROTOCOL_VERSION + 41).await;
    let (op_code, _) = next_frame(&mut out).await;
    assert_eq!(op_code, op::base::ERROR);

    send_auth(&tx, PROTOCOL_VERSION).await;
    let (op_code, payload) = next_frame(&mut out).await;
    assert_eq!(
        op_code,
        op::base::AUTH_RESULT,
        "the state must still be WaitingAuth after a version rejection"
    );
    let r = base::AuthResult::decode(payload.as_ref()).expect("AuthResult decode");
    assert!(r.ok, "auth rejected: {}", r.reason);
}

/// Adding `protocol_version` changed NO bytes for a client that does not
/// set it: proto3 omits a default-valued scalar, which is the whole
/// reason the field could be added to a shipped message at all.
#[test]
fn the_new_field_costs_nothing_on_the_wire_when_unset() {
    let legacy = Auth {
        name: "neo".into(),
        ticket: vec![],
        protocol_version: 0,
    };
    assert_eq!(
        legacy.encode_to_vec(),
        // field 1 (name), LEN 3, "neo" — and nothing else. This is
        // byte-for-byte what the pre-field `Auth` encoded.
        vec![0x0A, 0x03, b'n', b'e', b'o'],
        "an unset protocol_version must not appear on the wire"
    );

    // And when it IS set it is field 3, a varint — additive, nothing
    // before it moves.
    let versioned = Auth {
        name: "neo".into(),
        ticket: vec![],
        protocol_version: 1,
    };
    assert_eq!(
        versioned.encode_to_vec(),
        vec![0x0A, 0x03, b'n', b'e', b'o', 0x18, 0x01],
    );
}
