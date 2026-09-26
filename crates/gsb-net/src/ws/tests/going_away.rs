//! The server's own teardown close (BACKLOG B24): when the connection
//! actor ends the session — `stop()`, or any verdict whose `ERROR` frame
//! goes out first — the door says goodbye with status 1001 "Going Away",
//! never with an empty close frame (which a client reads as 1005, "no
//! status": indistinguishable from a peer that said nothing).

use super::*;
use gsb_core::channel::{Inbox, Mailbox};

/// The wire bytes of the teardown close, exactly: FIN + close opcode, an
/// unmasked 2-byte payload, status 1001 in network order, no reason.
const GOING_AWAY_FRAME: [u8; 4] = [0x88, 0x02, 0x03, 0xE9];

/// A door whose actor side the test holds: the endpoint's pumps run over
/// `out_tx` (what the actor would send) and `in_rx` (what it would read).
async fn door() -> (FakeWsClient, Mailbox<FrameBatch>, Inbox<ConnIn>) {
    let transport: Arc<dyn Transport> = Arc::new(WsTransport::default());
    let listener = transport
        .bind(SocketAddr::from(([127, 0, 0, 1], 0)))
        .await
        .expect("bind");
    let addr = listener.local_addr().unwrap();
    let client = tokio::spawn(FakeWsClient::connect(addr));
    let endpoint = listener.accept().await.expect("ws accept");
    let (in_tx, in_rx) = channel::<ConnIn>(8);
    let (out_tx, out_rx) = channel::<FrameBatch>(8);
    // The pump handles detach on drop; the tasks run on.
    let _ = endpoint.start_pump(
        ConnectionId(61),
        in_tx,
        out_rx,
        crate::pump::PumpTimeouts::default(),
    );
    (client.await.expect("client handshake"), out_tx, in_rx)
}

/// The actor's last frame (a stop notice's stand-in), then the actor is
/// gone: the client reads the frame, then the 1001 close — byte for byte
/// — and, once it answers the close, the end of the stream (no second
/// close frame).
#[tokio::test]
async fn the_server_teardown_closes_with_1001_going_away() {
    let (mut c, out_tx, _in_rx) = door().await;
    let notice = FrameBody::new(9, vec![0x08, 0x0E]);
    out_tx
        .send(vec![notice.clone()])
        .await
        .expect("the writer pump drains");
    drop(out_tx);
    let last = c.read_game().await;
    assert_eq!(
        (last.op, last.payload),
        (notice.op, notice.payload),
        "the last frame goes first"
    );
    let (fin, opcode, payload) = c.read_frame().await;
    let mut frame = vec![(u8::from(fin) << 7) | opcode, payload.len() as u8];
    frame.extend_from_slice(&payload);
    assert_eq!(frame, GOING_AWAY_FRAME, "the teardown close frame");
    // The client answers the close (RFC 6455 §5.5.1); the server, having
    // closed first, does not echo it — the stream just ends.
    c.send_frame(true, OP_CLOSE, &payload, true).await;
    c.expect_eof().await;
}

/// The code is a sendable one (RFC 6455 §7.4.1) and the frame is a
/// legal control frame: at most 125 payload bytes (status + a reason of
/// at most 123 UTF-8 bytes — here none).
#[test]
fn the_going_away_frame_is_a_legal_close() {
    assert_eq!(CLOSE_GOING_AWAY, 1001);
    let payload = CLOSE_GOING_AWAY.to_be_bytes();
    assert!(payload.len() <= MAX_CONTROL_PAYLOAD);
    assert_eq!(
        encode_server_frame(OP_CLOSE, &payload),
        GOING_AWAY_FRAME.to_vec()
    );
}
