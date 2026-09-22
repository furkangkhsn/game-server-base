//! RFC 6455 behavior over a live socket: fragmentation, control
//! frames, the rejections, and the close handshake. Child of `tests`,
//! so the echo-server helpers are shared, not duplicated.

use super::*;
use crate::transport::Transport;
use gsb_core::channel::FrameBatch;
use gsb_core::conn::ConnIn;
use gsb_core::id::ConnectionId;
use std::net::SocketAddr;
use std::sync::Arc;

/// Handshake succeeds with the RFC-vector accept key, and masked binary
/// game frames echo back one-for-one — including one big enough to force
/// the 64-bit length form in BOTH directions.
#[tokio::test]
async fn handshake_and_masked_game_frames_echo() {
    let addr = serve_echo(None).await;
    let mut client = FakeWsClient::connect(addr).await;

    client
        .send_game(&FrameBody::new(42, b"hello".as_slice()))
        .await;
    let echo = client.read_game().await;
    assert_eq!(echo.op, 42);
    assert_eq!(echo.payload.as_ref(), b"hello");

    let big_payload: Vec<u8> = (0..70_000u32).map(|i| i as u8).collect();
    client
        .send_game(&FrameBody::new(7, big_payload.clone()))
        .await;
    let echo = client.read_game().await;
    assert_eq!(echo.op, 7);
    assert_eq!(echo.payload.as_ref(), big_payload.as_slice());
}

/// A binary message fragmented into three frames reassembles into ONE
/// game frame (continuation opcodes, final FIN).
#[tokio::test]
async fn fragmented_binary_message_reassembles() {
    let addr = serve_echo(None).await;
    let mut client = FakeWsClient::connect(addr).await;

    let envelope = encode_game_envelope(&FrameBody::new(9, b"fragmented-game-frame".as_slice()));
    let mid = envelope.len() / 3;
    let (first, rest) = envelope.split_at(mid);
    let (second, third) = rest.split_at(rest.len() / 2);
    client.send_frame(false, OP_BIN, first, true).await;
    client.send_frame(false, OP_CONT, second, true).await;
    client.send_frame(true, OP_CONT, third, true).await;

    let echo = client.read_game().await;
    assert_eq!(echo.op, 9);
    assert_eq!(echo.payload.as_ref(), b"fragmented-game-frame");
}

/// Ping → Pong with the same application data, no game frame yielded.
#[tokio::test]
async fn ping_gets_pong() {
    let addr = serve_echo(None).await;
    let mut client = FakeWsClient::connect(addr).await;

    client.send_frame(true, OP_PING, b"hb", true).await;
    let (_, opcode, payload) = client.read_frame().await;
    assert_eq!(opcode, OP_PONG);
    assert_eq!(payload, b"hb");

    // And the data plane still works after the control exchange.
    client
        .send_game(&FrameBody::new(1, b"still-alive".as_slice()))
        .await;
    let echo = client.read_game().await;
    assert_eq!(echo.payload.as_ref(), b"still-alive");
}

/// An unmasked client frame is a protocol violation: the server fails
/// the connection with close code 1002 and shuts the socket down.
#[tokio::test]
async fn unmasked_client_frame_is_rejected_with_1002() {
    let addr = serve_echo(None).await;
    let mut client = FakeWsClient::connect(addr).await;

    let envelope = encode_game_envelope(&FrameBody::new(1, b"sneaky".as_slice()));
    client.send_ws_binary_unmasked(&envelope).await;

    assert_eq!(client.expect_close_then_eof().await, 1002);
}

/// Text messages have no meaning in the gsb wire contract → 1003.
#[tokio::test]
async fn text_message_is_rejected_with_1003() {
    let addr = serve_echo(None).await;
    let mut client = FakeWsClient::connect(addr).await;

    client.send_frame(true, OP_TEXT, b"hi", true).await;
    assert_eq!(client.expect_close_then_eof().await, 1003);
}

/// An oversized declared length fails fast — before its payload even
/// arrives — with close code 1009.
#[tokio::test]
async fn oversized_declared_length_is_rejected_with_1009() {
    let addr = serve_echo_max(64, None).await;
    let mut client = FakeWsClient::connect(addr).await;

    // Header claims 1000 payload bytes (over the 64 ceiling); we do not
    // even bother sending them all: rejection happens at header time.
    let mut frame = encode_client_frame(true, OP_BIN, &vec![0u8; 1000], [1, 2, 3, 4], true);
    frame.truncate(2 + 2 + 4 + 16); // head + ext16 + mask + partial payload
    client.send_raw(&frame).await;

    assert_eq!(client.expect_close_then_eof().await, 1009);
}

/// Client-initiated close: the server echoes the same status code, then
/// ends the stream, and the pump reports a clean "peer closed".
#[tokio::test]
async fn close_handshake_echoes_code_and_reports_peer_closed() {
    let transport: Arc<dyn Transport> = Arc::new(WsTransport::default());
    let addr = SocketAddr::from(([127, 0, 0, 1], 0));
    let listener = transport.bind(addr).await.expect("bind");
    let addr = listener.local_addr().unwrap();

    // Connect in the background; the backlog holds the socket until
    // accept runs (same pattern as the idle-timeout test).
    let client = tokio::spawn(FakeWsClient::connect(addr));

    let endpoint = listener.accept().await.expect("ws accept");
    let (in_tx, mut in_rx) = channel::<ConnIn>(8);
    // `out_tx` stays alive until after the echo is read: dropping it
    // makes the writer pump emit the transport's own (empty) close,
    // which would race the client-initiated one.
    let (out_tx, out_rx) = channel::<FrameBatch>(8);
    let (read, write) = endpoint.start_pump(
        ConnectionId(5),
        in_tx,
        out_rx,
        crate::pump::PumpTimeouts::default(),
    );
    tokio::spawn(async move {
        if let Some(read) = read {
            let _ = read.await;
        }
        let _ = write.await;
    });

    let mut client = client.await.expect("client handshake");
    client
        .send_frame(true, OP_CLOSE, &1000u16.to_be_bytes(), true)
        .await;

    let (fin, opcode, payload) = client.read_frame().await;
    assert!(fin && opcode == OP_CLOSE, "close echo expected");
    assert_eq!(payload, 1000u16.to_be_bytes(), "status code must be echoed");
    client.expect_eof().await;

    let msg = tokio::time::timeout(Duration::from_secs(5), in_rx.recv())
        .await
        .expect("inbox open")
        .expect("pump notified");
    match msg {
        ConnIn::Closed { reason } => assert_eq!(reason, "peer closed"),
        other => panic!("expected Closed, got {other:?}"),
    }
    drop(out_tx); // now the transport's own empty close may go out
}

/// The idle window still guards a WS connection: silence beyond the
/// window produces `ServerClosed` (idle reason), exactly like tcp.rs —
/// the WS reader simply pends like any other pump source.
#[tokio::test]
async fn idle_timeout_still_applies_to_websockets() {
    let transport: Arc<dyn Transport> = Arc::new(WsTransport::default());
    let addr = SocketAddr::from(([127, 0, 0, 1], 0));
    let listener = transport.bind(addr).await.expect("bind");
    let addr = listener.local_addr().unwrap();

    // The handshake only completes once accept runs, so connect in the
    // background first and let the TCP backlog hold the connection.
    let client = tokio::spawn(FakeWsClient::connect(addr));

    let endpoint = listener.accept().await.expect("ws accept");
    let (in_tx, mut in_rx) = channel::<ConnIn>(8);
    let (out_tx, out_rx) = channel::<FrameBatch>(8);
    let (read, write) = endpoint.start_pump(
        ConnectionId(6),
        in_tx,
        out_rx,
        crate::pump::PumpTimeouts {
            idle: Some(Duration::from_millis(200)),
            write_stall: None,
        },
    );
    // Hold the connection OPEN (binding matters: dropping it would send
    // a FIN and look like a clean close) and say nothing — after the
    // window the reader pump must fire ServerClosed on its own.
    let _client = client.await.expect("client handshake");

    let msg = tokio::time::timeout(Duration::from_secs(5), in_rx.recv())
        .await
        .expect("inbox open")
        .expect("pump notified");
    match msg {
        ConnIn::ServerClosed { cause, reason } => {
            assert_eq!(cause, gsb_core::conn::ServerClose::IdleTimeout);
            assert!(reason.contains("idle timeout"), "reason: {reason}");
        }
        other => panic!("expected ServerClosed, got {other:?}"),
    }
    read.expect("reader pump exits").await.unwrap();
    drop(out_tx);
    write.await.expect("writer pump exits");
}

/// A WebSocket protocol violation is the transport REFUSING the stream:
/// besides the 1002 close frame on the wire (pinned above), the reader
/// pump reports `StreamRejected` — the server's verdict — and not
/// `Closed`, which the client-initiated close handshake above pins as
/// the peer leaving.
#[tokio::test]
async fn a_protocol_violation_is_a_stream_rejection() {
    let transport: Arc<dyn Transport> = Arc::new(WsTransport::default());
    let listener = transport
        .bind(SocketAddr::from(([127, 0, 0, 1], 0)))
        .await
        .expect("bind");
    let client = tokio::spawn(FakeWsClient::connect(listener.local_addr().unwrap()));
    let endpoint = listener.accept().await.expect("ws accept");
    let (in_tx, mut in_rx) = channel::<ConnIn>(8);
    let (_out_tx, out_rx) = channel::<FrameBatch>(8);
    let (_read, _write) = endpoint.start_pump(
        ConnectionId(7),
        in_tx,
        out_rx,
        crate::pump::PumpTimeouts::default(),
    );
    let mut client = client.await.expect("client handshake");
    let envelope = encode_game_envelope(&FrameBody::new(1, b"sneaky".as_slice()));
    client.send_ws_binary_unmasked(&envelope).await;

    let msg = tokio::time::timeout(Duration::from_secs(5), in_rx.recv())
        .await
        .expect("the reader pump reported the rejection")
        .expect("inbox open");
    assert!(
        matches!(msg, ConnIn::StreamRejected { .. }),
        "a protocol violation is the server's verdict, not a peer close: {msg:?}"
    );
}

impl FakeWsClient {
    /// Test-only helper: deliberately unmasked binary message.
    async fn send_ws_binary_unmasked(&mut self, body: &[u8]) {
        self.send_frame(true, OP_BIN, body, false).await;
    }
}
