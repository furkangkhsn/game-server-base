//! The reader's control replies lost on a full control queue (BACKLOG
//! B58): before, a pong or a close frame the queue refused was dropped
//! with `let _ =`, counted nowhere. Now each is counted by kind and the
//! counts reach the collector when the reader goes.

use super::rig::ReaderRig;
use super::*;
use gsb_core::metrics::MetricsEvent;

/// A one-slot control queue nobody drains: the first ping's pong takes
/// it, the second ping's pong and the close echo are dropped. When the
/// reader is dropped (its pump ending), its counts go out.
#[tokio::test]
async fn a_pong_and_a_close_echo_dropped_on_a_full_queue_are_counted() {
    let (tx, mut rx) = tokio::sync::mpsc::channel(8);
    let mut rig = ReaderRig::with_queue(1, Some(tx)).await;
    rig.send(true, OP_PING, b"a").await;
    rig.send(true, OP_PING, b"b").await;
    rig.send(true, OP_CLOSE, &1000u16.to_be_bytes()).await;
    assert!(rig.next().await.is_none(), "the close ends the stream");
    rig.end();
    let Ok(MetricsEvent::Transport(t)) = rx.try_recv() else {
        panic!("the reader's counts, sent as it went");
    };
    assert_eq!(t.ws_pongs_dropped, 1, "the second pong");
    assert_eq!(t.ws_close_frames_dropped, 1, "the close echo");
    assert_eq!(t.handshakes_refused, 0, "nothing else");
    assert!(rx.try_recv().is_err(), "one sample");
}

/// Nothing lost, nothing sent: a reader whose replies all fit sends no
/// sample at all.
#[tokio::test]
async fn a_reader_that_lost_nothing_sends_nothing() {
    let (tx, mut rx) = tokio::sync::mpsc::channel(8);
    let mut rig = ReaderRig::with_queue(8, Some(tx)).await;
    rig.send(true, OP_PING, b"a").await;
    rig.send(true, OP_CLOSE, &1000u16.to_be_bytes()).await;
    assert!(rig.next().await.is_none());
    rig.end();
    assert!(rx.try_recv().is_err());
}
