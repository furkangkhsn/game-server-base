//! RFC 6455 §5.5.1 at the socket writer: once a Close frame is on the
//! wire, no DATA frame follows it — whoever queues one afterwards.
//!
//! Who does: the connection actor. A refused stream is reported to it as
//! `StreamRejected` AFTER the reader has queued its failure close, and
//! the actor answers every server verdict with a close notice (`ERROR`
//! code 9) — a data frame, necessarily queued behind the close frame.
//! The room's fan-out can land there too, in the window before the
//! teardown reaches it. Only the socket-writer task sees the real wire
//! order, so it is where the rule lives.

use super::*;

/// The actor-layer order on a refused stream exactly: the failure close
/// is queued, the pump reports the rejection, and a data frame is queued
/// after it. The client sees the close frame, then the end — nothing in
/// between.
#[tokio::test]
async fn no_data_frame_follows_a_close_frame() {
    let transport: Arc<dyn Transport> = Arc::new(WsTransport::default());
    let listener = transport
        .bind(SocketAddr::from(([127, 0, 0, 1], 0)))
        .await
        .expect("bind");
    let addr = listener.local_addr().unwrap();
    let client = tokio::spawn(FakeWsClient::connect(addr));
    let endpoint = listener.accept().await.expect("ws accept");
    let (in_tx, mut in_rx) = channel::<ConnIn>(8);
    let (out_tx, out_rx) = channel::<FrameBatch>(8);
    let (_read, _write) = endpoint.start_pump(
        ConnectionId(54),
        in_tx,
        out_rx,
        crate::pump::PumpTimeouts::default(),
    );
    let mut c = client.await.expect("client handshake");
    // A text message is outside the wire contract: failure close 1003.
    c.send_frame(true, OP_TEXT, b"hi", true).await;
    loop {
        match tokio::time::timeout(Duration::from_secs(5), in_rx.recv()).await {
            Ok(Some(ConnIn::StreamRejected { .. })) => break,
            Ok(Some(_)) => {}
            other => panic!("the rejection was never reported: {other:?}"),
        }
    }
    // The actor's close notice, queued behind the close frame.
    out_tx
        .send(vec![FrameBody::new(9, vec![0x08, 0x09])])
        .await
        .expect("the writer pump still drains");
    drop(out_tx);
    assert_eq!(c.expect_close_then_eof().await, 1003);
}
