//! The game band's feedback over real sockets: the writer probes a
//! session only once its client announced, and turns the reports into
//! the session's loss and RTT counters; a reporting `UdpClient` against
//! the real door is probed and answers; a client with reports off is
//! the client that predates them — never probed.

use super::*;

use gsb_core::channel::{FrameBatch, channel};
use gsb_core::conn::ConnIn;
use gsb_core::metrics::{MetricsEvent, TransportCounters};

fn game() -> FrameBody {
    FrameBody::new(1003, Bytes::from_static(b"snap"))
}

fn report(id: u32, received: u32) -> FrameBody {
    FrameBody::new(
        gsb_protocol::op::base::UDP_REPORT,
        Bytes::from(encode_report(id, received)[1..].to_vec()),
    )
}

/// The next datagram the peer received (bounded wait), skipping the
/// reliable band's (a control frame nobody ACKs is re-sent meanwhile).
async fn next(sock: &UdpSocket) -> Vec<u8> {
    let mut buf = [0u8; 2048];
    loop {
        let (n, _) = tokio::time::timeout(Duration::from_secs(3), sock.recv_from(&mut buf))
            .await
            .expect("a datagram")
            .expect("recv");
        if buf[0] != KIND_REL {
            return buf[..n].to_vec();
        }
    }
}

/// The writer, driven as the demux and the room drive it: no probe before
/// the announcement, a probe at once after it and an interval later, and
/// the reports' counts — 8 game datagrams sent (the control frame among
/// them is not one), the client missing 2 of the last 5 — in the
/// session's counters.
#[tokio::test]
async fn the_writer_probes_an_announced_session_and_counts_its_reports() {
    let (metrics_tx, mut metrics_rx) = mpsc::channel(64);
    let (in_tx, _inbox) = channel::<ConnIn>(8);
    let (out_tx, out_rx) = channel::<FrameBatch>(8);
    let client = UdpSocket::bind("127.0.0.1:0").await.expect("bind");
    let sock = Arc::new(UdpSocket::bind("127.0.0.1:0").await.expect("bind"));
    let (reaper, _reap_rx) = Reaper::new(sock.clone());
    let spawn = udp_pump_spawner(
        sock,
        client.local_addr().unwrap(),
        1200,
        reaper,
        Some(metrics_tx),
        UdpCongestion::Off,
    );
    let w = spawn(
        ConnectionId(5),
        in_tx,
        out_rx,
        crate::pump::PumpTimeouts::default(),
    )
    .1;
    // Two housekeeping ticks pass first: an unannounced session gets no
    // probe on them.
    tokio::time::sleep(RETRANSIT_TICK * 3).await;
    out_tx.send(vec![game(), game(), game()]).await.unwrap();
    for _ in 0..3 {
        assert_eq!(
            next(&client).await[0],
            KIND_RAW,
            "no probe before the announcement"
        );
    }
    out_tx.send(vec![report(0, 3)]).await.unwrap();
    assert_eq!(
        next(&client).await,
        encode_probe(1, 0),
        "the first probe, at once"
    );
    let control = FrameBody::new(
        gsb_protocol::op::base::HEARTBEAT_ACK,
        Bytes::from_static(&[1]),
    );
    let mut batch = vec![game(); 5];
    batch.insert(2, control);
    out_tx.send(batch).await.unwrap();
    for _ in 0..5 {
        assert_eq!(next(&client).await[0], KIND_RAW);
    }
    out_tx.send(vec![report(1, 3)]).await.unwrap();
    let probe = next(&client).await; // an interval later
    assert_eq!(probe[0], KIND_PROBE);
    let (id, echo) = parse_two_u32(&probe[1..]).unwrap();
    assert_eq!(id, 2);
    assert!(echo > 0, "it echoes probe 1's round trip");
    out_tx.send(vec![report(2, 6)]).await.unwrap();
    drop(out_tx);
    tokio::time::timeout(Duration::from_secs(5), w)
        .await
        .expect("the writer ended")
        .expect("no panic");
    let mut t = TransportCounters::default();
    while let Some(ev) = metrics_rx.recv().await {
        if let MetricsEvent::Transport(d) = ev {
            t.add(&d);
        }
    }
    assert_eq!(t.udp_game_announces_received, 1);
    assert_eq!(
        (t.udp_game_probes_sent, t.udp_game_reports_received),
        (2, 2)
    );
    assert_eq!(t.udp_game_datagrams_reported_sent, 8);
    assert_eq!(t.udp_game_datagrams_reported_lost, 2);
    assert_eq!(t.udp_game_rtt_samples, 2);
    assert!(t.udp_game_rtt_sum_us >= u64::from(echo));
    assert_eq!(t.udp_game_probes_unanswered, 0);
    assert_eq!(t.udp_game_reports_invalid + t.udp_game_reports_late, 0);
    assert_eq!(
        t.udp_frames_unsent + t.udp_frames_drained,
        0,
        "reports are no frames"
    );
}

/// The sum of the transport samples received so far, waiting (at most
/// `within`) until `done` holds.
async fn collect(
    rx: &mut mpsc::Receiver<MetricsEvent>,
    within: Duration,
    done: impl Fn(&TransportCounters) -> bool,
) -> TransportCounters {
    let mut t = TransportCounters::default();
    let deadline = tokio::time::Instant::now() + within;
    while !done(&t) {
        match tokio::time::timeout_at(deadline, rx.recv()).await {
            Ok(Some(MetricsEvent::Transport(d))) => t.add(&d),
            Ok(Some(_)) => {}
            Ok(None) | Err(_) => break,
        }
    }
    t
}

/// A session on the real door, the room sending a snapshot every 50 ms
/// while the client reads, until `until` holds for the client.
async fn run_session(
    config: UdpClientConfig,
    until: impl Fn(&UdpClient) -> bool,
    within: Duration,
) -> (UdpClient, mpsc::Receiver<MetricsEvent>, Arc<dyn Listener>) {
    let (metrics_tx, metrics_rx) = mpsc::channel(256);
    let cfg = UdpTransportConfig {
        metrics: Some(metrics_tx),
        ..Default::default()
    };
    let (listener, addr, mut eps, _accept) = bound_transport(cfg).await;
    let mut c = UdpClient::connect_with(addr, config)
        .await
        .expect("connect");
    let mut ep = tokio::time::timeout(Duration::from_secs(3), eps.recv())
        .await
        .expect("endpoint")
        .expect("endpoint");
    let (in_tx, _in_rx) = ep.take_inbox(16);
    let (out_tx, out_rx) = ep.take_outbox(16);
    let _pump = ep.start_pump(
        ConnectionId(9),
        in_tx,
        out_rx,
        crate::pump::PumpTimeouts::default(),
    );
    let deadline = tokio::time::Instant::now() + within;
    while !until(&c) && tokio::time::Instant::now() < deadline {
        let _ = out_tx.try_send(vec![game()]);
        let _ = c.recv_frame(Duration::from_millis(50)).await;
    }
    (c, metrics_rx, listener)
}

/// A reporting client (the default) on the real door: it announces, is
/// probed once a second, answers — and the server measures the path.
#[tokio::test]
async fn a_reporting_client_is_probed_and_the_path_measured() {
    let (c, mut rx, listener) = run_session(
        UdpClientConfig::default(),
        |c| c.stats.reports_sent >= 2,
        Duration::from_secs(10),
    )
    .await;
    assert!(c.stats.reports_sent >= 2, "{:?}", c.stats);
    assert!(c.stats.announces_sent >= 1);
    let t = collect(&mut rx, Duration::from_secs(5), |t| {
        t.udp_game_reports_received >= 2
    })
    .await;
    assert!(t.udp_game_announces_received >= 1, "{t:?}");
    assert!(t.udp_game_reports_received >= 2, "{t:?}");
    assert!(t.udp_game_rtt_samples >= 2);
    assert!(t.udp_game_datagrams_reported_sent > 0, "snapshots flowed");
    assert_eq!(t.udp_game_reports_invalid, 0);
    listener.close();
}

/// Reports off — the client before they existed: it never announces, so
/// the server never probes it; the snapshots flow exactly as before.
#[tokio::test]
async fn a_client_that_does_not_report_is_never_probed() {
    let (c, mut rx, listener) = run_session(
        UdpClientConfig {
            game_reports: false,
            ..Default::default()
        },
        |c| c.stats.game_datagrams_received >= 30,
        Duration::from_secs(10),
    )
    .await;
    // 30 snapshots at one per 50 ms: past a whole probe interval.
    assert!(c.stats.game_datagrams_received >= 30, "{:?}", c.stats);
    assert_eq!((c.stats.probes_received, c.stats.announces_sent), (0, 0));
    listener.close();
    let t = collect(&mut rx, Duration::from_millis(1500), |_| false).await;
    assert_eq!(
        (t.udp_game_announces_received, t.udp_game_probes_sent),
        (0, 0),
        "{t:?}"
    );
}
