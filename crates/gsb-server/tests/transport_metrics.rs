//! The rUDP door's own losses reach the server's metrics report (BACKLOG
//! B58): the composition root gives the demux the collector's channel.
//! Two datagrams of an unknown kind, the second past the demux's flush
//! interval: both counted as malformed in the report. A plaintext door
//! (`udp_security = "plaintext"`): on a sealed one the same bytes are
//! `udp_datagrams_unsealed` (B5a) — the demux's unit tests count those.

use std::time::Duration;

use gsb_server::{ListenerEntry, ListenerTransport};
use tokio::net::UdpSocket;

#[tokio::test]
async fn the_rudp_doors_drops_reach_the_report() {
    let cfg = gsb_server::Config {
        room_count: 1,
        listeners: Some(vec![ListenerEntry {
            transport: ListenerTransport::Udp,
            bind: "127.0.0.1:0".into(),
            tls_cert: None,
            tls_key: None,
        }]),
        udp_security: gsb_server::UdpSecurityKind::Plaintext,
        ..Default::default()
    };
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let handle = gsb_server::start_server_metrics(cfg, tx)
        .await
        .expect("server starts");
    let addr = handle.addrs[0];
    let sock = UdpSocket::bind("127.0.0.1:0").await.expect("bind");
    // Kind 0xEE is no datagram kind: dropped, counted malformed.
    sock.send_to(&[0xEE, 1, 2, 3], addr).await.expect("send");
    tokio::time::sleep(Duration::from_millis(600)).await;
    sock.send_to(&[0xEE, 4, 5, 6], addr).await.expect("send");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let report = tokio::time::timeout_at(deadline, rx.recv())
            .await
            .expect("the drops reported in time")
            .expect("metrics channel open");
        if report.transport.udp_datagrams_malformed == 2 {
            break;
        }
    }
    handle.stop().await;
}
