//! The QUIC door fills a session's path (B103): a server that writes
//! tells its connection actor what quinn measured — through the inbox,
//! as a `ConnIn::Path`, an open path with its round trip.

use super::*;

#[tokio::test]
async fn a_writing_session_tells_its_actor_its_path() {
    let pki = mint_pki("path");
    let listener = Arc::new(transport_for(&pki))
        .bind("127.0.0.1:0".parse().unwrap())
        .await
        .expect("bind");
    let bound = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let endpoint = listener.accept().await.expect("accept + handshake");
        let (in_tx, mut in_rx) = channel::<ConnIn>(8);
        let (out_tx, out_rx) = channel::<FrameBatch>(8);
        let (read, write) = endpoint.start_pump(
            ConnectionId(51),
            in_tx,
            out_rx,
            crate::pump::PumpTimeouts::default(),
        );
        // The client speaks first; the server's answer moves bytes.
        let first = in_rx.recv().await.expect("inbox open");
        assert!(matches!(first, ConnIn::Frame(_)), "{first:?}");
        out_tx
            .send(vec![FrameBody::new(7, vec![1u8; 256])])
            .await
            .expect("writer alive");
        let path = match tokio::time::timeout(Duration::from_secs(5), in_rx.recv()).await {
            Ok(Some(ConnIn::Path(p))) => p,
            other => panic!("the path news was expected after a write: {other:?}"),
        };
        drop(out_tx);
        let _ = write.await;
        if let Some(r) = read {
            r.abort();
        }
        path
    });
    let (mut reader, mut writer) = connect(bound, "localhost", pki.ca_pem.as_bytes())
        .await
        .expect("connect");
    writer
        .send(FrameBody::new(7, b"hi".as_slice()))
        .await
        .expect("client send");
    let _ = reader.next().await;
    let path = server.await.expect("server task");
    assert_eq!(path.phase, gsb_core::path::PathPhase::Open);
    assert!(path.rtt.is_some(), "quinn's round trip: {path:?}");
    assert_eq!(path.rate, None, "an open path carries no budget");
    drop(writer);
    drop(reader);
}
