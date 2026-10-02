//! The server's teardown close (1001 "Going Away") into a full or closed
//! socket-writer queue (BACKLOG B80). Before, `poll_close` was a
//! `try_send` that dropped it uncounted on a full queue: the client got
//! no close at all. Now it waits for a slot (the pump bounds the wait by
//! its stall window) and what still cannot go is counted — a closed
//! queue, a close abandoned while waiting. The reader's own close, queued
//! first or meanwhile, is the connection's one close: nothing counted.

use super::*;

use std::sync::atomic::{AtomicBool, Ordering};

use futures::SinkExt;
use gsb_core::metrics::{MetricsEvent, TransportCounters};
use tokio::sync::mpsc;

/// The door's pump-and-socket half, end to end (a real socket writer).
mod wire;

/// The close's payload: status 1001, no reason.
const GOING_AWAY: [u8; 2] = [0x03, 0xE9];

fn game() -> WsOut {
    WsOut::Game(Bytes::from_static(&[0, 0, 0, 2, 7, 0]))
}

/// A writer over `tx` whose losses go to the returned receiver.
fn writer(
    tx: mpsc::Sender<WsOut>,
    closing: &Arc<AtomicBool>,
) -> (WsWriter, mpsc::Receiver<MetricsEvent>) {
    let (metrics, samples) = mpsc::channel(8);
    let w = WsWriter::new(
        tx,
        WsMessageMapping::GameEnvelope,
        Arc::clone(closing),
        crate::wire::WireCount::new(),
    )
    .with_metrics(Some(metrics));
    (w, samples)
}

/// A queue of two, full of game frames.
fn full_queue() -> (mpsc::Sender<WsOut>, mpsc::Receiver<WsOut>) {
    let (tx, rx) = mpsc::channel(2);
    tx.try_send(game()).expect("room");
    tx.try_send(game()).expect("room");
    (tx, rx)
}

fn is_going_away(out: Option<WsOut>) -> bool {
    matches!(out, Some(WsOut::Control(OP_CLOSE, p)) if p == GOING_AWAY)
}

async fn sample(rx: &mut mpsc::Receiver<MetricsEvent>) -> TransportCounters {
    match tokio::time::timeout(Duration::from_secs(5), rx.recv()).await {
        Ok(Some(MetricsEvent::Transport(t))) => t,
        other => panic!("a transport sample: {other:?}"),
    }
}

async fn settle() {
    for _ in 0..8 {
        tokio::task::yield_now().await;
    }
}

/// The queue is full at the teardown: the close waits for a slot and
/// then goes behind the frames queued ahead of it. Nothing is lost.
#[tokio::test]
async fn a_full_queue_delays_the_teardown_close_instead_of_dropping_it() {
    let (tx, mut rx) = full_queue();
    let closing = Arc::new(AtomicBool::new(false));
    let (mut w, mut samples) = writer(tx, &closing);
    let close = tokio::spawn(async move {
        w.close().await.expect("the close");
        w
    });
    settle().await;
    assert!(!close.is_finished(), "the close waits for a slot");
    assert!(matches!(rx.recv().await, Some(WsOut::Game(_))));
    let w = close.await.expect("the close task");
    assert!(closing.load(Ordering::SeqCst), "the close is claimed");
    assert!(matches!(rx.recv().await, Some(WsOut::Game(_))));
    assert!(is_going_away(rx.recv().await), "the close, last");
    drop(w);
    assert!(rx.recv().await.is_none(), "nothing behind it");
    assert!(samples.try_recv().is_err(), "nothing lost");
}

/// The socket writer had already stopped (a failed socket write closed
/// its queue): the close cannot be queued — counted as closed.
#[tokio::test]
async fn a_closed_queue_counts_the_teardown_close() {
    let (tx, rx) = mpsc::channel(2);
    drop(rx);
    let closing = Arc::new(AtomicBool::new(false));
    let (mut w, mut samples) = writer(tx, &closing);
    w.close().await.expect("the close");
    let t = sample(&mut samples).await;
    assert_eq!(t.ws_going_away_unsent_closed, 1, "{t:?}");
    assert_eq!(t.ws_going_away_unsent_stalled, 0);
    drop(w);
    assert!(samples.try_recv().is_err(), "counted once");
}

/// The close is dropped still waiting for a slot (the pump gave up at its
/// stall window): counted as stalled when the writer goes.
#[tokio::test]
async fn a_close_abandoned_waiting_for_a_slot_is_counted() {
    let (tx, _rx) = full_queue();
    let closing = Arc::new(AtomicBool::new(false));
    let (mut w, mut samples) = writer(tx, &closing);
    let waited = tokio::time::timeout(Duration::from_millis(50), w.close()).await;
    assert!(waited.is_err(), "the close waits for a slot");
    drop(w);
    let t = sample(&mut samples).await;
    assert_eq!(t.ws_going_away_unsent_stalled, 1, "{t:?}");
    assert_eq!(t.ws_going_away_unsent_closed, 0);
}

/// The reader's close was queued first, or while the teardown close
/// waited: that one is the connection's close — the teardown close is not
/// sent, and not counted, whether a slot frees, the queue closes, or the
/// writer goes.
#[tokio::test]
async fn the_reader_s_close_wins_and_nothing_is_counted() {
    // Queued first: nothing to send, so nothing to wait for either.
    let (tx, mut rx) = full_queue();
    let closing = Arc::new(AtomicBool::new(true));
    let (mut w, mut samples) = writer(tx, &closing);
    tokio::time::timeout(Duration::from_millis(50), w.close())
        .await
        .expect("no wait for a slot")
        .expect("the close");
    drop(w);
    assert!(matches!(rx.recv().await, Some(WsOut::Game(_))));
    assert!(matches!(rx.recv().await, Some(WsOut::Game(_))));
    assert!(rx.recv().await.is_none(), "no second close");
    assert!(samples.try_recv().is_err());

    // Meanwhile, then a slot frees: the reserved slot is given back.
    let (tx, mut rx) = full_queue();
    let closing = Arc::new(AtomicBool::new(false));
    let (mut w, mut samples) = writer(tx, &closing);
    let close = tokio::spawn(async move {
        w.close().await.expect("the close");
        w
    });
    settle().await;
    closing.store(true, Ordering::SeqCst);
    assert!(matches!(rx.recv().await, Some(WsOut::Game(_))));
    drop(close.await.expect("the close task"));
    assert!(matches!(rx.recv().await, Some(WsOut::Game(_))));
    assert!(rx.recv().await.is_none(), "no teardown close");
    assert!(samples.try_recv().is_err());

    // Meanwhile, then the queue closes (the peer's close handshake).
    let (tx, rx) = full_queue();
    let closing = Arc::new(AtomicBool::new(false));
    let (mut w, mut samples) = writer(tx, &closing);
    let waited = tokio::time::timeout(Duration::from_millis(50), w.close()).await;
    assert!(waited.is_err(), "the close waits for a slot");
    closing.store(true, Ordering::SeqCst);
    drop(rx);
    w.close().await.expect("the close");
    drop(w);
    assert!(samples.try_recv().is_err(), "nothing counted");
}

/// The socket writer stops early (here: the peer's close handshake) while
/// a sender holds a slot it reserved before: the frame it then sends into
/// the closed queue is still counted — the drain waits for it.
#[tokio::test]
async fn a_frame_sent_on_a_slot_reserved_before_the_drain_is_counted() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (ours, theirs) = tokio::join!(TcpStream::connect(addr), listener.accept());
    let _peer = theirs.unwrap().0;
    let (_r, w) = ours.unwrap().into_split();
    let (metrics, mut samples) = mpsc::channel(8);
    let (tx, _written) = spawn_socket_writer(w, Some(metrics));
    let slot = tx.clone().reserve_owned().await.expect("a slot");
    tx.try_send(WsOut::Shutdown).expect("room");
    // The writer meets the shutdown and starts its drain.
    settle().await;
    slot.send(game());
    drop(tx);
    let t = sample(&mut samples).await;
    assert_eq!(t.stream_frames_unwritten, 1, "{t:?}");
}
