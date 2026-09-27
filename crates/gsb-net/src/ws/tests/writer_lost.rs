//! What the door's socket writer never puts on the wire is counted
//! (BACKLOG B66): the game frames behind a close frame it already sent,
//! and what is still queued when it stops early — game frames with the
//! stream doors' unwritten frames, control frames on their own.

use super::*;

use gsb_core::metrics::{MetricsEvent, TransportCounters};
use tokio::sync::mpsc;

/// A connected pair; the test keeps the far end open and unread.
async fn pair() -> (TcpStream, TcpStream) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (ours, theirs) = tokio::join!(TcpStream::connect(addr), listener.accept());
    (ours.unwrap(), theirs.unwrap().0)
}

fn game() -> WsOut {
    WsOut::Game(Bytes::from_static(&[0, 0, 0, 2, 7, 0]))
}

async fn sample(rx: &mut mpsc::Receiver<MetricsEvent>) -> TransportCounters {
    match tokio::time::timeout(Duration::from_secs(5), rx.recv()).await {
        Ok(Some(MetricsEvent::Transport(t))) => t,
        other => panic!("a transport sample: {other:?}"),
    }
}

/// Game frames queued behind a close frame are dropped by the RFC rule
/// and counted as such; a control frame there is the rule, not a loss.
#[tokio::test]
async fn game_frames_behind_a_sent_close_are_counted() {
    let (ours, _peer) = pair().await;
    let (_r, w) = ours.into_split();
    let (metrics_tx, mut metrics_rx) = mpsc::channel(8);
    let (tx, _written) = spawn_socket_writer(w, Some(metrics_tx));
    tx.try_send(WsOut::Control(OP_CLOSE, vec![3, 233])).unwrap();
    tx.try_send(game()).unwrap();
    tx.try_send(game()).unwrap();
    tx.try_send(WsOut::Control(OP_PONG, Vec::new())).unwrap();
    drop(tx);
    let t = sample(&mut metrics_rx).await;
    assert_eq!(t.ws_frames_dropped_after_close, 2, "{t:?}");
    assert_eq!(t.stream_frames_unwritten, 0);
    assert_eq!(t.ws_control_frames_unwritten, 0);
}

/// The peer's close handshake stops the writer with frames still queued
/// behind the shutdown and no close of its own sent: they are unwritten.
#[tokio::test]
async fn frames_queued_behind_a_shutdown_are_counted_by_kind() {
    let (ours, _peer) = pair().await;
    let (_r, w) = ours.into_split();
    let (metrics_tx, mut metrics_rx) = mpsc::channel(8);
    let (tx, _written) = spawn_socket_writer(w, Some(metrics_tx));
    // All queued before the writer task first runs (one thread, no await
    // in between): it meets the shutdown first.
    tx.try_send(WsOut::Shutdown).unwrap();
    tx.try_send(game()).unwrap();
    tx.try_send(game()).unwrap();
    tx.try_send(game()).unwrap();
    tx.try_send(WsOut::Control(OP_PONG, Vec::new())).unwrap();
    let t = sample(&mut metrics_rx).await;
    assert_eq!(t.stream_frames_unwritten, 3, "{t:?}");
    assert_eq!(t.ws_control_frames_unwritten, 1);
    assert_eq!(t.ws_frames_dropped_after_close, 0);
    // The queue is closed: a late frame is refused, not lost silently.
    assert!(tx.try_send(game()).is_err(), "the queue is closed");
}
