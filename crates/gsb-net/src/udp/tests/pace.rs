//! The congestion response on a real socket, the test playing the room
//! and the demux (the client's reports are scripted): a client that does
//! not report gets the very bytes the writer always sent; a reporting
//! one whose reports say the path cannot keep up is paced — the oldest
//! game frames dropped whole and counted, the control band neither
//! dropped nor queued, and every queued frame sent, dropped or unsent.

use super::*;

use gsb_core::channel::{FrameBatch, Mailbox, channel};
use gsb_core::conn::ConnIn;
use gsb_core::metrics::{MetricsEvent, TransportCounters};
use gsb_protocol::op;

mod measure;
mod relay;

const BUDGET: usize = 1200;

fn raw(len: usize) -> FrameBody {
    FrameBody::new(1003, Bytes::from(vec![3u8; len]))
}

/// A game frame of three FRAG datagrams at [`BUDGET`], tagged.
fn big(tag: u8) -> FrameBody {
    FrameBody::new(1004, Bytes::from(vec![tag; 3000]))
}

fn control() -> FrameBody {
    FrameBody::new(op::base::HEARTBEAT_ACK, Bytes::from_static(&[1, 2]))
}

fn report(id: u32, received: u32) -> FrameBody {
    FrameBody::new(
        op::base::UDP_REPORT,
        Bytes::from(encode_report(id, received)[1..].to_vec()),
    )
}

/// One writer towards a fresh client socket (its actor's inbox held by
/// the caller: the session stays alive).
async fn writer(
    mode: UdpCongestion,
) -> (
    UdpSocket,
    Mailbox<FrameBatch>,
    tokio::task::JoinHandle<()>,
    mpsc::Receiver<MetricsEvent>,
    gsb_core::channel::Inbox<ConnIn>,
) {
    let (metrics_tx, metrics_rx) = mpsc::channel(256);
    let client = UdpSocket::bind("127.0.0.1:0").await.expect("bind");
    let sock = Arc::new(UdpSocket::bind("127.0.0.1:0").await.expect("bind"));
    let (reaper, _reap_rx) = Reaper::new(sock.clone());
    let peer = client.local_addr().unwrap();
    let spawn = udp_pump_spawner(sock, peer, BUDGET, reaper, Some(metrics_tx), mode);
    let (in_tx, inbox) = channel::<ConnIn>(8);
    let (out_tx, out_rx) = channel::<FrameBatch>(64);
    let w = spawn(ConnectionId(8), in_tx, out_rx, Default::default()).1;
    (client, out_tx, w, metrics_rx, inbox)
}

/// The next datagram (bounded wait — a positive read, never silence).
async fn next(sock: &UdpSocket) -> Vec<u8> {
    let mut buf = [0u8; 2048];
    let (n, _) = tokio::time::timeout(Duration::from_secs(3), sock.recv_from(&mut buf))
        .await
        .expect("a datagram")
        .expect("recv");
    buf[..n].to_vec()
}

/// The next datagram that is not a probe.
async fn next_frame(sock: &UdpSocket) -> Vec<u8> {
    loop {
        let d = next(sock).await;
        if d[0] != KIND_PROBE {
            return d;
        }
    }
}

/// The next game-band datagram (not a probe, not the control frame's
/// re-sends — nobody ACKs it here).
async fn next_game(sock: &UdpSocket) -> Vec<u8> {
    loop {
        let d = next_frame(sock).await;
        if d[0] != KIND_REL {
            return d;
        }
    }
}

/// The next probe's id (frames before it are dropped by the caller's
/// design: none are in flight).
async fn next_probe(sock: &UdpSocket) -> u32 {
    loop {
        let d = next(sock).await;
        if d[0] == KIND_PROBE {
            return parse_two_u32(&d[1..]).unwrap().0;
        }
    }
}

/// Every sample until the writer's last.
async fn totals(
    w: tokio::task::JoinHandle<()>,
    rx: &mut mpsc::Receiver<MetricsEvent>,
) -> TransportCounters {
    tokio::time::timeout(Duration::from_secs(5), w)
        .await
        .expect("the writer ended")
        .expect("no panic");
    let mut t = TransportCounters::default();
    while let Some(ev) = rx.recv().await {
        if let MetricsEvent::Transport(d) = ev {
            t.add(&d);
        }
    }
    t
}

/// A client that never reports, on a door with the response on: the
/// datagrams are the ones the response-off writer sends, byte for byte
/// and in order — RAW, FRAG and control alike — and nothing is paced.
#[tokio::test]
async fn a_client_that_does_not_report_gets_the_same_bytes() {
    let batch = || vec![raw(10), big(1), control(), raw(500), big(2), raw(1)];
    let mut seen = Vec::new();
    for mode in [UdpCongestion::Off, UdpCongestion::Pace] {
        let (client, out_tx, w, mut rx, _inbox) = writer(mode).await;
        let mut got = Vec::new();
        for _ in 0..20 {
            out_tx.send(batch()).await.unwrap();
            for _ in 0..10 {
                got.push(next(&client).await);
            }
        }
        drop(out_tx);
        let t = totals(w, &mut rx).await;
        assert_eq!(t.udp_game_frames_queued_paced, 0, "{mode:?}");
        assert_eq!(t.udp_game_probes_sent, 0);
        seen.push(got);
    }
    assert_eq!(seen[0].len(), 200);
    assert_eq!(seen[0], seen[1], "the same datagrams, in the same order");
}

/// Reports that say most of the band is lost, twice in a row: the
/// session is paced. A flood of fragmented frames then keeps only the
/// newest (the rest dropped whole, counted); the control frame in the
/// flood goes at once, ahead of the queued game band; the newest frame
/// goes out whole, at the paced rate; and at the end every queued frame
/// was sent, dropped or unsent.
#[tokio::test]
async fn a_paced_session_keeps_the_newest_and_never_queues_control() {
    let (client, out_tx, w, mut rx, _inbox) = writer(UdpCongestion::Pace).await;
    out_tx.send(vec![report(0, 0)]).await.unwrap();
    assert_eq!(next_probe(&client).await, 1, "the first probe, at once");
    out_tx.send(vec![report(1, 0)]).await.unwrap();
    let mut received = 0;
    for id in 2..=3 {
        out_tx.send(vec![raw(100); 20]).await.unwrap();
        for _ in 0..20 {
            assert_eq!(next_game(&client).await[0], KIND_RAW, "open: at once");
        }
        assert_eq!(next_probe(&client).await, id);
        received += 5; // 15 of each 20 "lost"
        out_tx.send(vec![report(id, received)]).await.unwrap();
    }
    // Paced now, at the floor (4 budgets per second: 4800 B/s).
    let mut flood: Vec<_> = (1..=10).map(big).collect();
    flood.push(control());
    out_tx.send(flood).await.unwrap();
    assert_eq!(next_frame(&client).await[0], KIND_REL, "control: at once");
    let mut frags = Vec::new();
    let mut at = Vec::new();
    for _ in 0..3 {
        frags.push(next_game(&client).await);
        at.push(std::time::Instant::now());
    }
    for (i, d) in frags.iter().enumerate() {
        assert_eq!(
            &d[..5],
            &[KIND_FRAG, d[1], d[2], i as u8, 3],
            "whole, in order"
        );
        assert!(
            d[5..].iter().skip(2).all(|&b| b == 10),
            "the newest: frame 10"
        );
    }
    // The bucket held one datagram: the other two went at 4800 B/s.
    let paced = Duration::from_secs_f64((frags[1].len() + frags[2].len()) as f64 / 4800.0);
    assert!(
        at[2] - at[0] >= paced.mul_f64(0.9),
        "paced: {paced:?} expected, {:?}",
        at[2] - at[0]
    );
    // A second flood, and the session ends at once: the newest is cut.
    out_tx.send((1..=5).map(big).collect()).await.unwrap();
    drop(out_tx);
    let t = totals(w, &mut rx).await;
    assert_eq!(t.udp_game_paced_episodes, 1);
    assert!(t.udp_game_paced_rate_cuts >= 1);
    assert_eq!(t.udp_game_frames_queued_paced, 15);
    assert_eq!(t.udp_game_frames_dropped_paced, 9 + 4);
    assert_eq!(t.udp_game_frames_unsent_paced, 1);
    assert_eq!(t.udp_frames_unsent + t.udp_frames_drained, 0);
}
