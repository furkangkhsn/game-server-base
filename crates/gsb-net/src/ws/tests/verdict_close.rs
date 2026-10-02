//! The teardown close's code at the door, by how the session ended
//! (BACKLOG B30): the connection actor's `SessionEnd` reaches the door
//! through the endpoint's end notice, and the close frame after the
//! actor's last frame carries the code — 1008 for a policy verdict, 1013
//! for a capacity refusal, 1001 for the stop and when nothing was told
//! (B24's bytes, unchanged).

use super::*;
use gsb_core::conn::{ServerClose, SessionEnd};

/// A door whose actor side the test plays: the end notice the endpoint
/// hands out, the outbound channel the actor would hold, the client.
async fn door() -> (
    FakeWsClient,
    Option<gsb_core::conn::EndNotice>,
    gsb_core::channel::Mailbox<FrameBatch>,
) {
    let transport: Arc<dyn Transport> = Arc::new(WsTransport::default());
    let listener = transport
        .bind(SocketAddr::from(([127, 0, 0, 1], 0)))
        .await
        .expect("bind");
    let addr = listener.local_addr().unwrap();
    let client = tokio::spawn(FakeWsClient::connect(addr));
    let mut endpoint = listener.accept().await.expect("ws accept");
    let notice = endpoint.take_end_notice();
    let (in_tx, _in_rx) = channel::<ConnIn>(8);
    let (out_tx, out_rx) = channel::<FrameBatch>(8);
    let _ = endpoint.start_pump(
        ConnectionId(62),
        in_tx,
        out_rx,
        crate::pump::PumpTimeouts::default(),
    );
    (client.await.expect("client handshake"), notice, out_tx)
}

/// The actor's last frame, its end told (or not), the actor gone: the
/// client reads the frame unchanged, then the close frame — returned as
/// its wire bytes.
async fn teardown(end: Option<SessionEnd>) -> Vec<u8> {
    let (mut c, notice, out_tx) = door().await;
    let notice = notice.expect("the WebSocket door asks how the session ended");
    if let Some(end) = end {
        notice.send(end).expect("the door listens");
    } else {
        drop(notice);
    }
    let last = FrameBody::new(9, vec![0x08, 0x09]);
    out_tx.send(vec![last.clone()]).await.expect("drained");
    drop(out_tx);
    let got = c.read_game().await;
    assert_eq!((got.op, got.payload), (last.op, last.payload));
    let (fin, opcode, payload) = c.read_frame().await;
    let mut frame = vec![(u8::from(fin) << 7) | opcode, payload.len() as u8];
    frame.extend_from_slice(&payload);
    frame
}

/// FIN + close, an unmasked 2-byte payload: the status code only.
fn close(code: u16) -> Vec<u8> {
    let [hi, lo] = code.to_be_bytes();
    vec![0x88, 0x02, hi, lo]
}

#[tokio::test]
async fn a_policy_verdict_closes_with_1008() {
    for verdict in [
        ServerClose::ViolationBudget,
        ServerClose::Kicked,
        ServerClose::IdleInput,
        ServerClose::IdleTimeout,
    ] {
        let got = teardown(Some(SessionEnd::Verdict(verdict))).await;
        assert_eq!(got, close(1008), "{verdict:?}");
    }
}

#[tokio::test]
async fn a_capacity_refusal_closes_with_1013() {
    let got = teardown(Some(SessionEnd::Verdict(ServerClose::ConnCap))).await;
    assert_eq!(got, close(1013));
}

#[tokio::test]
async fn the_stop_and_an_untold_end_keep_1001() {
    assert_eq!(teardown(Some(SessionEnd::Stopped)).await, close(1001));
    assert_eq!(teardown(None).await, close(1001));
}
