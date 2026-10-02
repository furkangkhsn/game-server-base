//! The writer's half of connection migration (module `crate::udp::path`):
//! after the demux's `UDP_PATH` notice every datagram goes to the new
//! address; a new IP starts the path estimate over (RFC 9000 §9.4), a new
//! port alone keeps it.

use super::*;

fn moved_to(a: SocketAddr) -> FrameBody {
    FrameBody::new(
        op::base::UDP_PATH,
        Bytes::from(crate::udp::path::encode_addr(a)),
    )
}

/// A paced session with an RTT estimate (the reports of
/// `the_writer_follows_its_controller`).
async fn paced() -> (UdpWriter, UdpSocket) {
    let (mut w, client) = writer(UdpCongestion::Pace).await;
    w.apply_report(&report(0, 0));
    w.probe_pass();
    w.apply_report(&report(1, 0));
    interval(&mut w, 2, 5);
    interval(&mut w, 3, 10);
    assert_eq!(w.path_state().phase, PathPhase::Paced);
    assert!(w.rel.rto().srtt().is_some() && w.game_estimate().is_some());
    (w, client)
}

/// A new port (a NAT rebinding): the writer sends there from now on —
/// a control frame and a probe both land on the new socket — and the
/// path's estimate is kept: the same path.
#[tokio::test]
async fn a_new_port_moves_the_sends_and_keeps_the_estimate() {
    let (mut w, _old) = paced().await;
    let srtt = w.rel.rto().srtt();
    let new = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let to = new.local_addr().unwrap();
    let batch = vec![
        moved_to(to),
        FrameBody::new(op::base::HEARTBEAT, Bytes::from_static(b"hb")),
    ];
    assert_eq!(w.send_batch(batch).await, None);
    assert_eq!(w.peer(), to);
    let mut buf = [0u8; 64];
    let (n, _) = tokio::time::timeout(Duration::from_secs(1), new.recv_from(&mut buf))
        .await
        .expect("the control frame, at the new address")
        .unwrap();
    assert_eq!(buf[0], KIND_REL, "{:?}", &buf[..n]);
    assert_eq!(w.rel.rto().srtt(), srtt, "kept");
    assert!(w.game_estimate().is_some());
    assert_eq!(w.path_state().phase, PathPhase::Paced);
    assert_eq!((w.path_changes, w.path_resets), (1, 0));
}

/// A new IP: the reliable band's estimator, the game band's estimate and
/// the congestion response start over — open, unpaced, at the normal
/// probe cadence.
#[tokio::test]
async fn a_new_ip_starts_the_path_estimate_over() {
    let (mut w, _old) = paced().await;
    let new = UdpSocket::bind("127.0.0.2:0")
        .await
        .expect("loopback alias");
    w.apply_path(&moved_to(new.local_addr().unwrap()));
    assert_eq!(w.rel.rto().srtt(), None, "the RTT estimator");
    assert_eq!(w.rel.rto().current(), crate::udp::rel::INITIAL_RTO);
    assert_eq!(w.game_estimate(), None, "the game band's estimate");
    assert_eq!(w.path_state(), PathState::default(), "open, unpaced");
    let every = crate::udp::feedback::PROBE_INTERVAL;
    let now = Instant::now();
    w.feedback.probe_sent(now);
    assert!(!w.feedback.probe_due(now + every / 2), "the normal cadence");
    assert_eq!((w.path_changes, w.path_resets), (1, 1));
}

/// The notice is the demux's, not the session's: never counted as one of
/// the session's frames (B66/B73's rule for the piggybacked kinds).
#[test]
fn the_notice_is_not_a_session_frame() {
    let f = moved_to("127.0.0.1:9".parse().unwrap());
    assert!(!send::is_session_frame(&f));
}
