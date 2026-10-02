//! B74: a session whose accept already went to the client but whose
//! endpoint the accept loop never took is counted when the listener
//! closes (the queue is dropped) — and an endpoint the accept loop did
//! take is not.

use super::*;

use gsb_core::metrics::{MetricsEvent, TransportCounters};

/// Every transport sample, summed until the last sender is gone (the
/// door's tasks stopped, every endpoint dropped) — a condition, not a
/// spell of silence (BACKLOG F75): nothing can arrive after it.
async fn summed(rx: &mut mpsc::Receiver<gsb_core::metrics::MetricsEvent>) -> TransportCounters {
    let mut total = TransportCounters::default();
    let all = tokio::time::timeout(Duration::from_secs(10), async {
        while let Some(ev) = rx.recv().await {
            if let MetricsEvent::Transport(t) = ev {
                total.add(&t);
            }
        }
    })
    .await;
    assert!(all.is_ok(), "every metrics sender gone: {total:?}");
    total
}

#[tokio::test]
async fn a_queued_session_the_close_drops_is_counted() {
    let (tx, mut rx) = mpsc::channel(64);
    let transport = Arc::new(UdpTransport {
        config: UdpTransportConfig {
            metrics: Some(tx),
            ..UdpTransportConfig::default()
        },
    });
    let listener = transport
        .bind("127.0.0.1:0".parse().unwrap())
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("local addr");

    // One session is taken by the accept loop, the next two are not.
    let raw = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    raw_handshake(&raw, addr, 1).await;
    let taken = tokio::time::timeout(Duration::from_secs(3), listener.clone().accept())
        .await
        .expect("the first endpoint")
        .expect("an endpoint");
    for nonce in [2, 3] {
        let raw = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        raw_handshake(&raw, addr, nonce).await;
    }
    listener.close();
    // The taken endpoint goes uncounted; with it and the listener go
    // the last metrics senders (`bind` consumed the transport).
    drop((taken, listener));
    let t = summed(&mut rx).await;
    assert_eq!(t.udp_sessions_unaccepted_closed, 2, "{t:?}");
    assert_eq!(t.udp_sessions_dropped_accept_gone, 0);
}
