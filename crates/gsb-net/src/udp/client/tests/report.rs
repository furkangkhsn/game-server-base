//! The client's half of the game band's feedback: it counts the game
//! datagrams, announces itself a bounded number of times, answers each
//! probe at once with its count, and takes the probe's echo as an RTT
//! sample — and with reports off it is the client that predates them.

use super::*;

/// The next datagram the silent peer received (bounded wait).
async fn next(sink: &UdpSocket) -> Vec<u8> {
    let mut buf = [0u8; 64];
    let (n, _) = tokio::time::timeout(Duration::from_secs(2), sink.recv_from(&mut buf))
        .await
        .expect("a datagram")
        .expect("recv");
    buf[..n].to_vec()
}

/// Nothing more reached the peer (what this client sends is in the
/// peer's queue the moment its send returns: loopback).
async fn nothing(sink: &UdpSocket) -> bool {
    let mut buf = [0u8; 64];
    tokio::time::timeout(Duration::from_millis(100), sink.recv_from(&mut buf))
        .await
        .is_err()
}

fn raw_game() -> Vec<u8> {
    encode_raw(&FrameBody::new(1003, Bytes::from_static(b"snap")))
}

/// A probe is answered at once with the game datagrams received so far
/// (RAW and FRAG alike), and its echo becomes the band's RTT sample.
#[tokio::test]
async fn a_probe_is_answered_with_the_count_and_its_echo_is_a_sample() {
    let (mut c, sink) = detached().await;
    for _ in 0..3 {
        c.process_datagram(&raw_game());
    }
    c.process_datagram(&[KIND_FRAG, 0, 0, 9, 9]); // a refused fragment: still delivered
    assert_eq!(c.srtt(), None);
    c.process_datagram(&encode_probe(5, 30_000));
    assert_eq!(next(&sink).await, encode_report(5, 4));
    assert_eq!(
        c.srtt(),
        Some(Duration::from_millis(30)),
        "the echo is a sample"
    );
    let s = &c.stats;
    assert_eq!(
        (s.game_datagrams_received, s.probes_received, s.reports_sent),
        (4, 1, 1)
    );
    // An echo of 0 is "no sample"; one past the liveness bound is refused.
    c.process_datagram(&encode_probe(6, 0));
    c.process_datagram(&encode_probe(7, 6_000_000));
    assert_eq!(next(&sink).await, encode_report(6, 4));
    assert_eq!(next(&sink).await, encode_report(7, 4));
    assert_eq!(c.stats.probe_echoes_refused, 1);
    assert_eq!(
        c.srtt(),
        Some(Duration::from_millis(30)),
        "neither moved it"
    );
}

/// The announcement goes once an interval, at most three times, and
/// stops at the first probe.
#[tokio::test]
async fn announcements_are_bounded_and_end_at_the_first_probe() {
    let (mut c, sink) = detached().await;
    let t0 = Instant::now();
    c.announce(t0);
    assert_eq!(next(&sink).await, encode_report(0, 0));
    c.announce(t0 + ANNOUNCE_EVERY / 2);
    assert!(nothing(&sink).await, "not before an interval");
    for k in 1..=ANNOUNCE_MAX {
        c.announce(t0 + ANNOUNCE_EVERY * k);
    }
    assert_eq!(next(&sink).await, encode_report(0, 0));
    assert_eq!(next(&sink).await, encode_report(0, 0));
    assert!(
        nothing(&sink).await,
        "three at most: an older server never probes"
    );
    assert_eq!(c.stats.announces_sent, u64::from(ANNOUNCE_MAX));

    let (mut c, sink) = detached().await;
    c.announce(t0);
    c.process_datagram(&encode_probe(1, 0));
    assert_eq!(next(&sink).await, encode_report(0, 0));
    assert_eq!(next(&sink).await, encode_report(1, 0));
    c.announce(t0 + ANNOUNCE_EVERY);
    assert!(nothing(&sink).await, "a probe ends the announcements");
}

/// Reports off: the client that predates them — no announcement, and a
/// probe (which such a client never gets) is ignored, not answered.
#[tokio::test]
async fn with_reports_off_a_probe_is_ignored() {
    let (mut c, sink) = detached_with(UdpClientConfig {
        game_reports: false,
        ..Default::default()
    })
    .await;
    c.announce(Instant::now());
    c.process_datagram(&encode_probe(1, 30_000));
    assert!(nothing(&sink).await, "nothing sent");
    assert_eq!(c.srtt(), None, "no sample taken");
    assert_eq!((c.stats.announces_sent, c.stats.reports_sent), (0, 0));
}
