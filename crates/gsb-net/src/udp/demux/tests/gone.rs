//! B74: a proof verified while the accept side is gone (the endpoint
//! receiver dropped) tears its session down at once — counted, and no
//! accept goes to the client.

use super::*;

use gsb_core::metrics::{MetricsEvent, TransportCounters};
use tokio::sync::mpsc;

#[tokio::test]
async fn a_session_the_accept_side_cannot_take_is_counted() {
    let sock = Arc::new(
        UdpSocket::bind("127.0.0.1:0".parse::<SocketAddr>().unwrap())
            .await
            .expect("bind"),
    );
    sock.writable().await.expect("writable");
    let (mut d, end_rx) = demux_bare(sock);
    let (tx, mut rx) = mpsc::channel(8);
    d.flusher = crate::metrics::Flusher::new(Some(tx));
    drop(end_rx);
    let peer_sock = UdpSocket::bind("127.0.0.1:0").await.expect("peer");
    let peer = peer_sock.local_addr().unwrap();
    let nonce = 0x6011_u64;
    let proof = encode_hello(nonce, d.cookie.compute(nonce, peer, d.clock.slot()));

    feed(&mut d, peer, &proof);
    assert!(d.sessions.is_empty(), "torn down at once");
    let mut buf = [0u8; 64];
    let answer =
        tokio::time::timeout(Duration::from_millis(200), peer_sock.recv_from(&mut buf)).await;
    assert!(answer.is_err(), "no accept went out: {answer:?}");

    d.flush_metrics(true);
    let Ok(MetricsEvent::Transport(t)) = rx.try_recv() else {
        panic!("a transport sample");
    };
    assert_eq!(
        t,
        TransportCounters {
            udp_sessions_dropped_accept_gone: 1,
            ..Default::default()
        }
    );
}

/// A send the FULL endpoint queue refuses is the demux's own count
/// (`udp_sessions_dropped_accept_full`), never also an unaccepted
/// session: the demux takes its endpoint back before dropping it.
#[tokio::test]
async fn a_session_the_full_queue_refuses_is_counted_once() {
    let sock = Arc::new(
        UdpSocket::bind("127.0.0.1:0".parse::<SocketAddr>().unwrap())
            .await
            .expect("bind"),
    );
    sock.writable().await.expect("writable");
    let (mut d, end_rx) = demux_bare(sock);
    let (tx, mut rx) = mpsc::channel(8);
    d.flusher = crate::metrics::Flusher::new(Some(tx.clone()));
    d.metrics = Some(tx);
    // The harness queue holds four; the fifth proof finds it full.
    for nonce in 0..5u64 {
        let peer_sock = UdpSocket::bind("127.0.0.1:0").await.expect("peer");
        let peer = peer_sock.local_addr().unwrap();
        let proof = encode_hello(nonce, d.cookie.compute(nonce, peer, d.clock.slot()));
        feed(&mut d, peer, &proof);
    }
    assert_eq!((end_rx.len(), d.endpoints_dropped), (4, 1));
    d.flush_metrics(true);
    let mut total = TransportCounters::default();
    while let Ok(MetricsEvent::Transport(t)) = rx.try_recv() {
        total.add(&t);
    }
    assert_eq!(
        total,
        TransportCounters {
            udp_sessions_dropped_accept_full: 1,
            ..Default::default()
        }
    );
    drop(end_rx);
}
