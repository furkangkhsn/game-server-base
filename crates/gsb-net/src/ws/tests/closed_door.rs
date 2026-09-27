//! B74: what a closing handshake door drops is counted and reaches the
//! collector in the intake's last sample — the upgrade it cut (a peer
//! that never sent its request) and the finished upgrade still queued
//! for an accept loop that never took it.

use super::*;
use crate::transport::Listener;
use crate::transport::intake::tests::until;
use gsb_core::metrics::{MetricsEvent, TransportCounters};

#[tokio::test]
async fn the_close_counts_the_cut_and_the_unaccepted() {
    let (tx, mut rx) = tokio::sync::mpsc::channel(8);
    let transport: Arc<dyn Transport> = Arc::new(WsTransport {
        metrics: Some(tx),
        ..WsTransport::default()
    });
    let listener: Arc<dyn Listener> = transport
        .bind(SocketAddr::from(([127, 0, 0, 1], 0)))
        .await
        .expect("bind");
    let addr = listener.local_addr().unwrap();
    let stats = || listener.handshake_stats().expect("a handshaking door");
    let _idle = FakeWsClient::raw(addr).await;
    let _upgraded = FakeWsClient::connect(addr).await;
    until("one queued, one in flight", || {
        let s = stats();
        (s.completed, s.in_flight) == (1, 2)
    })
    .await;

    listener.close();
    let s = stats();
    assert_eq!(s.unaccepted_closed, 1, "the queue dropped at the close");
    let mut total = TransportCounters::default();
    while let Ok(Some(ev)) = tokio::time::timeout(Duration::from_millis(500), rx.recv()).await {
        if let MetricsEvent::Transport(t) = ev {
            total.add(&t);
        }
    }
    assert_eq!(
        (
            total.handshakes_cut_closed,
            total.handshakes_unaccepted_closed
        ),
        (1, 1),
        "{total:?}"
    );
    assert_eq!(stats().cut_closed, 1);
}
