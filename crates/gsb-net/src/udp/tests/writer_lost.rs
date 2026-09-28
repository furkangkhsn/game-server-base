//! What the rUDP writer loses, counted (BACKLOG B66), and the band's
//! death verdict landing in a FULL mailbox: datagrams the socket refused
//! (by band), the frames never sent when the band dies (the rest of the
//! batch and the queue), and the notice that must explain the close.

use super::*;

use gsb_core::channel::{FrameBatch, channel};
use gsb_core::conn::{ConnIn, ServerClose};
use gsb_core::metrics::{MetricsEvent, TransportCounters};

async fn bound() -> Arc<UdpSocket> {
    Arc::new(
        UdpSocket::bind("127.0.0.1:0".parse::<SocketAddr>().unwrap())
            .await
            .expect("bind"),
    )
}

async fn transport_sample(rx: &mut mpsc::Receiver<MetricsEvent>) -> TransportCounters {
    match tokio::time::timeout(Duration::from_secs(5), rx.recv()).await {
        Ok(Some(MetricsEvent::Transport(t))) => t,
        other => panic!("a transport sample: {other:?}"),
    }
}

fn game() -> FrameBody {
    FrameBody::new(1000, Bytes::from_static(b"move"))
}

fn control(len: usize) -> FrameBody {
    FrameBody::new(op::base::HEARTBEAT_ACK, Bytes::from(vec![7u8; len]))
}

/// Start one writer on `sock` towards `peer` (the spawner the demux
/// hands the accept loop), with `in_tx` as the actor's mailbox.
fn writer(
    sock: Arc<UdpSocket>,
    peer: SocketAddr,
    max_datagram: usize,
    in_tx: gsb_core::channel::Mailbox<ConnIn>,
    out_rx: gsb_core::channel::Inbox<FrameBatch>,
    metrics: mpsc::Sender<MetricsEvent>,
) -> tokio::task::JoinHandle<()> {
    let (reaper, _reap_rx) = Reaper::new(sock.clone());
    let spawn = udp_pump_spawner(sock, peer, max_datagram, reaper, Some(metrics));
    spawn(
        ConnectionId(71),
        in_tx,
        out_rx,
        crate::pump::PumpTimeouts::default(),
    )
    .1
}

/// Datagrams the socket refuses (an IPv6 peer from an IPv4 socket fails
/// every send at once) are counted by band.
#[tokio::test]
async fn datagrams_the_socket_refuses_are_counted_by_band() {
    let (metrics_tx, mut metrics_rx) = mpsc::channel(8);
    let (in_tx, _inbox) = channel::<ConnIn>(8);
    let (out_tx, out_rx) = channel::<FrameBatch>(8);
    let peer = "[::1]:9".parse().unwrap();
    let w = writer(bound().await, peer, 1200, in_tx, out_rx, metrics_tx);
    out_tx.try_send(vec![game(), game()]).unwrap();
    out_tx.try_send(vec![control(2)]).unwrap();
    drop(out_tx);
    let t = transport_sample(&mut metrics_rx).await;
    assert_eq!(t.udp_game_datagrams_send_failed, 2, "{t:?}");
    assert_eq!(t.udp_control_datagrams_send_failed, 1, "{t:?}");
    assert_eq!(t.udp_frames_unsent, 0, "the band did not die");
    tokio::time::timeout(Duration::from_secs(5), w)
        .await
        .expect("the writer ended")
        .expect("no panic");
}

/// The band dies on an undeliverable control frame with the actor's
/// mailbox FULL: the notice still lands (the slot reserved at the
/// writer's birth), and the frames never sent — the fatal one, the rest
/// of its batch, the batch queued behind — are counted.
#[tokio::test]
async fn the_bands_death_counts_the_unsent_and_its_notice_lands_in_a_full_mailbox() {
    let (metrics_tx, mut metrics_rx) = mpsc::channel(8);
    let (in_tx, mut inbox) = channel::<ConnIn>(4);
    let (out_tx, out_rx) = channel::<FrameBatch>(8);
    let raw = UdpSocket::bind("127.0.0.1:0").await.expect("bind");
    let peer = raw.local_addr().unwrap();
    let w = writer(bound().await, peer, 40, in_tx.clone(), out_rx, metrics_tx);
    // The client keeps sending: the mailbox fills (one slot is the
    // writer's).
    let hb = || ConnIn::Frame(FrameBody::new(op::base::HEARTBEAT, Vec::new()));
    let mut queued = 0;
    while in_tx.try_send(hb()).is_ok() {
        queued += 1;
    }
    assert_eq!(queued, 3, "four slots, one reserved for the verdict");
    // One sendable control frame, then one over the 40-byte budget (the
    // band's death) with a game frame behind it, and a queued batch.
    out_tx
        .try_send(vec![control(2), control(40), game()])
        .unwrap();
    out_tx.try_send(vec![game(), game()]).unwrap();
    let t = transport_sample(&mut metrics_rx).await;
    assert_eq!(t.udp_frames_unsent, 2 + 2, "{t:?}");
    assert_eq!(t.udp_control_frames_abandoned, 1, "the first, never acked");
    assert_eq!(t.writer_verdicts_deferred, 0, "the slot was reserved");
    let mut notice = None;
    while let Ok(msg) = inbox.try_recv() {
        if let ConnIn::ServerClosed { cause, .. } = msg {
            notice = Some(cause);
        }
    }
    assert_eq!(notice, Some(ServerClose::RelDead), "the close is explained");
    assert!(
        out_tx.try_send(vec![game()]).is_err(),
        "the channel is closed"
    );
    tokio::time::timeout(Duration::from_secs(5), w)
        .await
        .expect("the writer ended")
        .expect("no panic");
}

/// A mailbox already full when the writer is born: no slot to reserve,
/// so the notice is delivered after the close — counted as deferred.
#[tokio::test]
async fn a_notice_without_a_reserved_slot_is_counted_as_deferred() {
    let (metrics_tx, mut metrics_rx) = mpsc::channel(8);
    let (in_tx, mut inbox) = channel::<ConnIn>(2);
    let hb = || ConnIn::Frame(FrameBody::new(op::base::HEARTBEAT, Vec::new()));
    in_tx.try_send(hb()).unwrap();
    in_tx.try_send(hb()).unwrap();
    let (out_tx, out_rx) = channel::<FrameBatch>(8);
    let raw = UdpSocket::bind("127.0.0.1:0").await.expect("bind");
    let peer = raw.local_addr().unwrap();
    let w = writer(bound().await, peer, 40, in_tx, out_rx, metrics_tx);
    out_tx.try_send(vec![control(40)]).unwrap();
    let t = transport_sample(&mut metrics_rx).await;
    assert_eq!(t.writer_verdicts_deferred, 1, "{t:?}");
    assert_eq!(t.udp_frames_unsent, 1);
    // Making room lets the late notice in.
    let _ = inbox.recv().await;
    tokio::time::timeout(Duration::from_secs(5), w)
        .await
        .expect("the writer ended")
        .expect("no panic");
    let mut causes = Vec::new();
    while let Ok(msg) = inbox.try_recv() {
        if let ConnIn::ServerClosed { cause, .. } = msg {
            causes.push(cause);
        }
    }
    assert_eq!(causes, vec![ServerClose::RelDead]);
}

/// B73: after the session is over, `udp_frames_drained` counts the
/// session's own frames the writer takes off its channel — game and
/// control frames — and not the demux's piggybacked ACKs (a transport
/// message for a band that is gone, not a frame of the session).
#[tokio::test]
async fn frames_drained_after_the_end_leave_the_piggybacked_acks_out() {
    let (metrics_tx, mut metrics_rx) = mpsc::channel(8);
    // The actor has exited: its mailbox is closed.
    let (in_tx, inbox) = channel::<ConnIn>(8);
    drop(inbox);
    let (out_tx, out_rx) = channel::<FrameBatch>(8);
    let raw = UdpSocket::bind("127.0.0.1:0").await.expect("bind");
    let peer = raw.local_addr().unwrap();
    let w = writer(bound().await, peer, 1200, in_tx, out_rx, metrics_tx);
    let ack = || FrameBody::new(op::base::UDP_ACK, Bytes::from(1u32.to_le_bytes().to_vec()));
    // The first batch is sent; with nothing outstanding the session is
    // then over. The second is drained: two frames and one ACK.
    out_tx.try_send(vec![game()]).unwrap();
    out_tx.try_send(vec![ack(), game(), control(2)]).unwrap();
    drop(out_tx);
    tokio::time::timeout(Duration::from_secs(5), w)
        .await
        .expect("the writer ended")
        .expect("no panic");
    let mut total = TransportCounters::default();
    while let Some(ev) = metrics_rx.recv().await {
        if let MetricsEvent::Transport(t) = ev {
            total.add(&t);
        }
    }
    assert_eq!(total.udp_frames_drained, 2, "{total:?}");
    assert_eq!(total.udp_frames_unsent, 0, "the band did not die");
}

/// The writer's control re-sends reach the collector by cause (B2):
/// every re-send the peer saw is counted as a timer expiry.
#[tokio::test]
async fn control_re_sends_are_counted_as_timer_expiries() {
    let (metrics_tx, mut metrics_rx) = mpsc::channel(64);
    let (in_tx, _inbox) = channel::<ConnIn>(8);
    let (out_tx, out_rx) = channel::<FrameBatch>(8);
    let sink = bound().await;
    let peer = sink.local_addr().unwrap();
    let w = writer(bound().await, peer, 1200, in_tx, out_rx, metrics_tx);
    out_tx.try_send(vec![control(2)]).unwrap();
    // The first send and two re-sends (at ~50 and ~150 ms), never ACKed.
    let mut buf = [0u8; 64];
    for _ in 0..3 {
        tokio::time::timeout(Duration::from_secs(5), sink.recv_from(&mut buf))
            .await
            .expect("a copy")
            .expect("recv");
    }
    drop(out_tx);
    tokio::time::timeout(Duration::from_secs(5), w)
        .await
        .expect("the writer ended")
        .expect("no panic");
    let mut counted = 0;
    while let Ok(MetricsEvent::Transport(t)) = metrics_rx.try_recv() {
        counted += t.udp_control_retransmits_timeout;
        assert_eq!(t.udp_control_datagrams_send_failed, 0, "{t:?}");
    }
    assert!(counted >= 2, "two re-sends seen, {counted} counted");
}
