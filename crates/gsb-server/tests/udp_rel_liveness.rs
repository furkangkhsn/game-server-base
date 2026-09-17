//! The rUDP reliable band's liveness bound, end to end.
//!
//! The bug this locks: the REL give-up used to be **silent**. A control
//! frame the peer never ACKed was popped after 250 ms and counted, which
//! wedges that direction's cumulative stream forever (the receiver never
//! advances past the hole) while the session stays alive and the RAW game
//! band keeps flowing. A client whose `JOIN_ROOM_RESULT` was the
//! abandoned frame waited forever, and the server kept the session — and
//! everything it holds — alive with it.
//!
//! The fix (`gsb-net/src/udp`, "The REL liveness bound"): a frame is
//! never abandoned on its own age; the BAND dies when the cumulative ACK
//! has made no progress at all for `REL_NO_ACK_FATAL` (5 s), and that
//! death is session-fatal. The writer hands `ConnIn::ServerClosed` to the
//! connection actor over its mailbox — an in-process channel, never the
//! socket — and the actor runs the ORDINARY teardown.
//!
//! This test forces the condition with peers that stop ACKing and asserts
//! the ordinary teardown really ran, on both of its shapes:
//!
//! - an authenticated-but-unaffiliated session: its registry row is
//!   released outright (`conns` drops);
//! - a session that is in a room: its close is routed as a DETACH to the
//!   room, whose `on_disconnect` policy then owns the entity and its slot
//!   (RECONNECT §4) — byte-for-byte what a dropped TCP socket does. The
//!   assertion is therefore that the close reached the registry
//!   (`closes`), which is the single exit of `ConnectionActor::run`.
//!
//! The demux's idle sweep is disabled (`idle_timeout_secs = 0`) so the
//! liveness bound is the ONLY mechanism that can satisfy any of it — and
//! the sweep watches INBOUND silence anyway, which is not what a peer
//! that keeps talking but never ACKs does.

use std::time::{Duration, Instant};

use gsb_core::metrics::{MetricReport, RegistryReport};
use gsb_protocol::base::{Auth, AuthResult, Error, Heartbeat, JoinRoom, JoinRoomResult};
use prost::Message;
use tokio::sync::mpsc;

/// A UDP server whose only session-death mechanism is the liveness bound.
fn cfg() -> gsb_server::Config {
    gsb_server::Config {
        bind: "127.0.0.1:0".into(),
        room_count: 1,
        transport: gsb_server::TransportKind::Udp,
        idle_timeout_secs: 0.0,
        ..Default::default()
    }
}

/// Drain metric reports until one carries a registry section satisfying
/// `done`, or the deadline passes (then panic with the last one seen).
async fn registry_until(
    rx: &mut mpsc::UnboundedReceiver<MetricReport>,
    what: &str,
    deadline: Instant,
    done: impl Fn(&RegistryReport) -> bool,
) -> RegistryReport {
    let mut last: Option<RegistryReport> = None;
    loop {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .unwrap_or_else(|| panic!("timed out waiting for {what}; last registry: {last:?}"));
        match tokio::time::timeout(remaining, rx.recv()).await {
            Ok(Some(report)) => {
                if let Some(reg) = report.registry {
                    if done(&reg) {
                        return reg;
                    }
                    last = Some(reg);
                }
            }
            Ok(None) => panic!("the metrics channel closed while waiting for {what}"),
            Err(_) => panic!("timed out waiting for {what}; last registry: {last:?}"),
        }
    }
}

/// Send one frame on the reliable control band.
async fn send(client: &mut gsb_net::udp::UdpClient, op: u16, payload: Vec<u8>) {
    client.send_frame(op, payload).await.expect("frame sent");
}

fn auth_of(name: &str) -> Vec<u8> {
    Auth {
        name: name.into(),
        ticket: vec![],
        protocol_version: gsb_protocol::PROTOCOL_VERSION,
    }
    .encode_to_vec()
}

/// Read (and therefore ACK) until `want` arrives.
async fn read_until(client: &mut gsb_net::udp::UdpClient, want: u16) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        let Some(frame) = client
            .recv_frame(Duration::from_millis(200))
            .await
            .expect("recv")
        else {
            continue;
        };
        match frame.op {
            gsb_protocol::op::base::AUTH_RESULT => {
                assert!(
                    AuthResult::decode(&frame.payload[..]).unwrap().ok,
                    "auth must succeed"
                );
            }
            gsb_protocol::op::base::JOIN_ROOM_RESULT => {
                assert!(JoinRoomResult::decode(&frame.payload[..]).unwrap().entity != 0);
            }
            gsb_protocol::op::base::ERROR => {
                let e = Error::decode(&frame.payload[..]).unwrap();
                panic!("unexpected ERROR: code={} message={}", e.code, e.message);
            }
            _ => {}
        }
        if frame.op == want {
            return;
        }
    }
    panic!("never saw op {want}");
}

/// Two peers that stop ACKing lose their sessions, and the ordinary
/// teardown runs for both: the registry sees both closes, and the row
/// that nothing parks is released. Before the fix neither session ever
/// ended — the undeliverable frame was dropped, a counter incremented,
/// and the server went on holding everything.
#[tokio::test]
async fn peers_that_stop_acking_lose_their_sessions_and_the_teardown_runs() {
    let (report_tx, mut reports) = mpsc::unbounded_channel();
    let handle = gsb_server::start_server_metrics(cfg(), report_tx)
        .await
        .expect("server starts");

    // A: authenticates and joins room 1 (its close routes a DETACH).
    let mut a = gsb_net::udp::UdpClient::connect(handle.addr)
        .await
        .expect("A handshake");
    send(&mut a, gsb_protocol::op::base::AUTH_REQ, auth_of("rel-a")).await;
    send(
        &mut a,
        gsb_protocol::op::base::JOIN_ROOM_REQ,
        JoinRoom { room_id: 1 }.encode_to_vec(),
    )
    .await;
    read_until(&mut a, gsb_protocol::op::base::JOIN_ROOM_RESULT).await;

    // B: authenticates only (nothing to park — its row is released).
    let mut b = gsb_net::udp::UdpClient::connect(handle.addr)
        .await
        .expect("B handshake");
    send(&mut b, gsb_protocol::op::base::AUTH_REQ, auth_of("rel-b")).await;
    read_until(&mut b, gsb_protocol::op::base::AUTH_RESULT).await;

    assert!(a.is_established() && b.is_established());
    assert_eq!(
        handle
            .room_status(gsb_core::id::RoomId(1))
            .await
            .expect("status"),
        gsb_core::registry::RoomStatus::Running { members: 1 },
        "A holds a room slot before the band dies"
    );
    let before = registry_until(
        &mut reports,
        "both connections to be registered",
        Instant::now() + Duration::from_secs(10),
        |r| r.conns == 2,
    )
    .await;
    assert_eq!(before.closes, 0, "nothing has closed yet: {before:?}");

    // Now provoke a server control frame on each and NEVER read again.
    // The HEARTBEAT_ACK is reliable, so it stays outstanding, is
    // retransmitted every RTO, and is never confirmed — the "peer that
    // never ACKs" condition, exactly like a one-way dead path.
    for c in [&mut a, &mut b] {
        send(
            c,
            gsb_protocol::op::base::HEARTBEAT,
            Heartbeat { tick: 1 }.encode_to_vec(),
        )
        .await;
    }

    // The bound is 5 s; allow generous slack for a loaded CI box. The
    // idle sweep is off, so nothing else can move these counters.
    let after = registry_until(
        &mut reports,
        "both sessions to end through the ordinary teardown",
        Instant::now() + Duration::from_secs(25),
        |r| r.closes >= 2,
    )
    .await;
    assert_eq!(
        after.closes, 2,
        "both connection actors must reach RegistryMsg::ConnClosed — the \
         single exit of ConnectionActor::run: {after:?}"
    );
    assert_eq!(
        after.conns, 1,
        "B's registry row (nothing parks an unaffiliated session) must be \
         released; A's is held by the room's disconnect policy: {after:?}"
    );

    handle.stop().await;
}
