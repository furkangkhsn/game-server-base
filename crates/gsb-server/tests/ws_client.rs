//! `gsb_client`'s WebSocket half against the REAL door (`gsb_net::ws`
//! behind the composition root): the upgrade, the session steps, and
//! the door's close frames surfaced with their status codes — its
//! protocol verdicts (1002 / 1003 / 1007 / 1009), the echo of a client
//! close, and the close after a stop.
//!
//! The scripted-server half of the suite (fragmentation, pings, the
//! guard, cancel safety — what the door never does on its own) lives in
//! `gsb-client`'s own tests.

use std::time::Duration;

use gsb_client::session::{self, Credentials};
use gsb_client::ws::{OP_BIN, OP_CLOSE, OP_CONT, OP_TEXT};
use gsb_client::{ClientError, Conn, Recv};
use gsb_protocol::base::ErrorCode;
use gsb_protocol::op::base as op;
use tokio::io::AsyncWriteExt;

const W: Duration = Duration::from_secs(10);

/// A one-room server with a single WebSocket door.
async fn server() -> gsb_server::ServerHandle {
    let cfg = gsb_server::Config {
        room_count: 1,
        listeners: Some(vec![gsb_server::ListenerEntry {
            transport: gsb_server::ListenerTransport::Ws,
            bind: "127.0.0.1:0".into(),
            tls_cert: None,
            tls_key: None,
        }]),
        ..Default::default()
    };
    gsb_server::start_server(cfg).await.expect("server starts")
}

/// A connected, authenticated WebSocket client.
async fn authed(handle: &gsb_server::ServerHandle, name: &str) -> Conn {
    let mut c = gsb_client::connect::ws(handle.addrs[0])
        .await
        .expect("the door upgrades");
    assert!(c.is_ws());
    session::auth(&mut c, &Credentials::named(name), W, |_| {})
        .await
        .expect("auth");
    c
}

/// Read to the end: every frame before it must be none (the door's close
/// frame is its only notice — RFC 6455 §5.5.1), and the end is `Closed`.
async fn read_to_close(c: &mut Conn, what: &str) {
    match c.recv(W).await {
        Ok(Recv::Closed) => {}
        other => panic!("{what}: want the close, got {other:?}"),
    }
}

/// Every step over the door: AUTH + JOIN, a HEARTBEAT round, LEAVE —
/// each frame one masked binary message, each reply one server message.
#[tokio::test]
async fn the_session_steps_run_over_websocket() {
    let handle = server().await;
    let mut c = gsb_client::connect::ws(handle.addrs[0])
        .await
        .expect("the door upgrades");
    let joined = session::auth_and_join(&mut c, &Credentials::named("ws-steps"), 1, W, |_| {})
        .await
        .expect("join");
    assert!(joined.auth.ok);
    assert_ne!(joined.entity, 0);
    let tick = session::heartbeat_round(&mut c, 41, W, |_| {})
        .await
        .expect("heartbeat");
    assert_eq!(tick, 41);
    session::leave(&mut c, W, |_| {}).await.expect("leave");
    assert!(c.ws_close().is_none(), "the session is still open");
    handle.stop().await;
}

/// The door's protocol verdicts reach the caller as the close frame's
/// status code: a text message 1003, a stray continuation 1002, a
/// message that is not one frame 1007, a message over the door's 1 MiB
/// ceiling 1009 (refused from its header: only the header is written).
#[tokio::test]
async fn the_doors_close_codes_are_surfaced() {
    let handle = server().await;
    let frame = gsb_client::frame::encode(7, b"x");
    let cases: [(&str, u8, &[u8], u16); 3] = [
        ("text", OP_TEXT, b"hi", 1003),
        ("stray continuation", OP_CONT, &frame, 1002),
        ("not one frame", OP_BIN, &frame[..5], 1007),
    ];
    for (what, opcode, payload, code) in cases {
        let mut c = authed(&handle, what).await;
        let Conn::Stream { tx, .. } = &mut c else {
            unreachable!()
        };
        tx.ws_frame(true, opcode, payload).await.expect("write");
        read_to_close(&mut c, what).await;
        assert_eq!(c.ws_close().and_then(|x| x.code), Some(code), "{what}");
    }
    let mut c = authed(&handle, "oversized").await;
    let Conn::Stream { tx, .. } = &mut c else {
        unreachable!()
    };
    // A masked binary header declaring 2 MiB, and its mask key: bytes
    // outside the frame contract, written under the WS framing.
    let mut head = vec![0x80 | OP_BIN, 0x80 | 127];
    head.extend_from_slice(&(2u64 << 20).to_be_bytes());
    head.extend_from_slice(&[1, 2, 3, 4]);
    tx.get_mut().write_all(&head).await.expect("write");
    read_to_close(&mut c, "oversized").await;
    assert_eq!(c.ws_close().and_then(|x| x.code), Some(1009));
    handle.stop().await;
}

/// A close the client starts: the door echoes its code and ends the
/// session.
#[tokio::test]
async fn a_client_close_is_echoed_by_the_door() {
    let handle = server().await;
    let mut c = authed(&handle, "ws-bye").await;
    let Conn::Stream { tx, .. } = &mut c else {
        unreachable!()
    };
    tx.ws_frame(true, OP_CLOSE, &1000u16.to_be_bytes())
        .await
        .expect("write");
    read_to_close(&mut c, "client close").await;
    assert_eq!(c.ws_close().and_then(|x| x.code), Some(1000));
    handle.stop().await;
}

/// A stop reaches a joined client as `ERROR` 14, then the door's close
/// frame ends the session — present, its code exposed. (The code is the
/// door's to choose: today the close is empty, `code: None`; a parallel
/// round (B24) makes it 1001 "going away" — the assertion accepts both
/// until that lands, then tightens to `Some(1001)`.)
#[tokio::test]
async fn a_stop_is_the_notice_then_the_close_frame() {
    let handle = server().await;
    let mut c = gsb_client::connect::ws(handle.addrs[0])
        .await
        .expect("the door upgrades");
    session::auth_and_join(&mut c, &Credentials::named("ws-stop"), 1, W, |_| {})
        .await
        .expect("join");
    tokio::time::timeout(W, handle.stop())
        .await
        .expect("stop() completes");
    let deadline = std::time::Instant::now() + W;
    let e = session::reply(&mut c, op::HEARTBEAT_ACK, deadline, &mut |_| {})
        .await
        .expect_err("the session ended");
    match &e {
        ClientError::Server(s) => assert_eq!((s.code, s.raw), (ErrorCode::ServerStopping, 14)),
        other => panic!("want the stop notice, got {other:?}"),
    }
    read_to_close(&mut c, "stop").await;
    let close = c.ws_close().expect("the door's close frame");
    assert_eq!(
        close.code,
        Some(1001),
        "a server-ended session closes with 1001 Going Away (B24): {close:?}"
    );
}
