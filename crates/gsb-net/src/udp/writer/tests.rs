//! The writer's game-band feedback, on a writer built but not spawned:
//! an answered probe is a sample of the reliable band's estimator
//! (BACKLOG B87) — it seeds a band that had none, and it ends a backoff
//! the way a clean ACK does.

use super::*;
use bytes::Bytes;
use gsb_core::channel::channel;
use gsb_protocol::{FrameBody, op};

fn report(id: u32, received: u32) -> FrameBody {
    FrameBody::new(
        op::base::UDP_REPORT,
        Bytes::from(encode_report(id, received)[1..].to_vec()),
    )
}

async fn writer() -> (UdpWriter, UdpSocket) {
    let sock = Arc::new(UdpSocket::bind("127.0.0.1:0").await.expect("bind"));
    sock.writable().await.expect("writable");
    let client = UdpSocket::bind("127.0.0.1:0").await.expect("bind");
    let (reaper, _reap_rx) = Reaper::new(sock.clone());
    let link = spawn::Link {
        sock,
        peer: client.local_addr().unwrap(),
        max_datagram: 1200,
        reaper,
        metrics: None,
    };
    let (in_tx, _inbox) = channel::<ConnIn>(8);
    let (_out_tx, out_rx) = channel::<FrameBatch>(8);
    (UdpWriter::new(link, ConnectionId(3), in_tx, out_rx), client)
}

#[tokio::test]
async fn an_answered_probe_is_a_sample_of_the_reliable_band() {
    let (mut w, client) = writer().await;
    w.apply_report(&report(0, 0));
    w.probe_pass();
    let mut buf = [0u8; 16];
    let (n, _) = tokio::time::timeout(Duration::from_secs(3), client.recv_from(&mut buf))
        .await
        .expect("the probe, at once")
        .expect("recv");
    assert_eq!(&buf[..n], &encode_probe(1, 0)[..]);
    // The band has a control frame out, never answered, re-sent twice:
    // no sample, a doubled-twice timer.
    w.rel.push(1, Bytes::from_static(b"x"), Instant::now());
    w.rel.resent(Instant::now());
    w.rel.resent(Instant::now());
    assert_eq!(w.rel.rto().srtt(), None);
    assert_eq!(w.rel.rto().current(), crate::udp::rel::INITIAL_RTO * 4);
    // Probe 1 answered: the band's first sample, and the backoff ends.
    w.apply_report(&report(1, 0));
    assert!(
        w.rel.rto().srtt().is_some(),
        "the probe's round trip is a sample"
    );
    assert_eq!(
        w.rel.rto().current(),
        crate::udp::rel::MIN_RTO,
        "backoff gone"
    );
    assert!(w.game_estimate().is_some());
}
