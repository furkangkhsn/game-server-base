//! The writer's game-band feedback, on a writer built but not spawned:
//! an answered probe is a sample of the reliable band's estimator
//! (BACKLOG B87) — it seeds a band that had none, and it ends a backoff
//! the way a clean ACK does. And the congestion response's wiring: the
//! controller follows the reports, the probes follow the controller, and
//! a ring of unanswered probes reaches it.

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

async fn writer(congestion: UdpCongestion) -> (UdpWriter, UdpSocket) {
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
        congestion,
    };
    let (in_tx, _inbox) = channel::<ConnIn>(8);
    let (_out_tx, out_rx) = channel::<FrameBatch>(8);
    (UdpWriter::new(link, ConnectionId(3), in_tx, out_rx), client)
}

#[tokio::test]
async fn an_answered_probe_is_a_sample_of_the_reliable_band() {
    let (mut w, client) = writer(UdpCongestion::Off).await;
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

/// One probe out at once, 20 game datagrams "sent" before it, and the
/// client's answer saying it got `got` of them.
fn interval(w: &mut UdpWriter, id: u32, got: u32) {
    for _ in 0..20 {
        w.feedback.game_sent(100);
    }
    w.feedback.set_interval(Duration::ZERO);
    w.probe_pass();
    w.apply_report(&report(id, got));
}

/// The writer feeds its controller every applied report, probes at the
/// controller's cadence, and hands it a ring of unanswered probes.
#[tokio::test]
async fn the_writer_follows_its_controller() {
    let (mut w, _client) = writer(UdpCongestion::Pace).await;
    w.apply_report(&report(0, 0));
    w.probe_pass();
    w.apply_report(&report(1, 0));
    interval(&mut w, 2, 5); // 15 of 20 lost
    assert_eq!(w.path_state().phase, PathPhase::Suspect);
    let fast = crate::udp::congestion::FAST_PROBE_INTERVAL;
    let now = Instant::now();
    assert!(!w.feedback.probe_due(now + fast / 2), "the fast cadence");
    assert!(w.feedback.probe_due(now + fast));
    interval(&mut w, 3, 10);
    assert_eq!(w.path_state().phase, PathPhase::Paced);
    assert_eq!(w.pace.control.counts.cuts, 1);
    w.feedback.set_interval(Duration::ZERO);
    for _ in 0..crate::udp::feedback::PROBE_RING {
        w.probe_pass();
    }
    assert_eq!(w.pace.control.counts.cuts, 1, "the ring is full, not over");
    w.probe_pass();
    assert_eq!(w.pace.control.counts.cuts, 2, "a ring unanswered: a cut");
}

/// With the response off, the same reports leave the session open: the
/// controller is never consulted.
#[tokio::test]
async fn a_writer_with_the_response_off_never_paces() {
    let (mut w, _client) = writer(UdpCongestion::Off).await;
    w.apply_report(&report(0, 0));
    w.probe_pass();
    w.apply_report(&report(1, 0));
    interval(&mut w, 2, 5);
    interval(&mut w, 3, 10);
    assert_eq!(w.path_state(), PathState::default());
    assert_eq!(
        w.pace_offer(vec![vec![0; 10]], false),
        Some(vec![vec![0; 10]])
    );
}

/// A cut trims the queue to the new rate's budget at once (not at the
/// next frame), and the writer then wakes for the pacer, not the tick.
#[tokio::test]
async fn a_cut_trims_the_queue_and_the_pacer_sets_the_wake() {
    let (mut w, _client) = writer(UdpCongestion::Pace).await;
    w.apply_report(&report(0, 0));
    w.probe_pass();
    w.apply_report(&report(1, 0));
    interval(&mut w, 2, 17); // 3 of 20 lost: a signal
    interval(&mut w, 3, 34);
    assert_eq!(w.path_state().phase, PathPhase::Paced);
    for _ in 0..10 {
        assert_eq!(w.pace_offer(vec![vec![0; 1000]], false), None, "queued");
    }
    assert_eq!(
        w.pace.queue.counts.dropped, 0,
        "a fast path's budget holds them"
    );
    interval(&mut w, 4, 34); // all 20 lost: down to the floor
    assert_eq!(w.pace.queue.counts.dropped, 9, "trimmed by the cut itself");
    assert!(
        w.wake(Instant::now()) < RETRANSIT_TICK,
        "the pacer's deadline"
    );
}

mod path;
