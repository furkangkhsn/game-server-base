//! The pumps' own losses reach the collector (BACKLOG B66): the frames a
//! writer never wrote when it ended on a failed write or a stall — the
//! rest of its batch and every batch still queued — and the frame a
//! reader could not hand to a closed inbox, by kind.

use super::*;

use futures::StreamExt;
use gsb_core::metrics::TransportCounters;

/// A socket that takes `ok` frames and then fails every write.
struct FailsAfter {
    ok: usize,
    sent: usize,
}

impl Sink<FrameBody> for FailsAfter {
    type Error = std::io::Error;

    fn poll_ready(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn start_send(self: Pin<&mut Self>, _item: FrameBody) -> std::io::Result<()> {
        self.get_mut().sent += 1;
        Ok(())
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();
        if this.sent > this.ok {
            return Poll::Ready(Err(std::io::ErrorKind::BrokenPipe.into()));
        }
        Poll::Ready(Ok(()))
    }

    fn poll_close(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

impl WriteProgress for FailsAfter {
    fn bytes_written(&self) -> u64 {
        self.sent as u64
    }
}

fn frames(n: usize) -> FrameBatch {
    (0..n).map(|_| FrameBody::new(0x7E00, vec![1, 2])).collect()
}

/// The next transport sample, within a few seconds.
async fn transport_sample(rx: &mut mpsc::Receiver<MetricsEvent>) -> TransportCounters {
    let got = tokio::time::timeout(Duration::from_secs(5), rx.recv())
        .await
        .expect("a transport sample in time");
    match got {
        Some(MetricsEvent::Transport(t)) => t,
        other => panic!("a transport sample: {other:?}"),
    }
}

/// A failed write: the failed frame and the one after it in its batch,
/// and the whole batch still queued behind it, never reach the socket.
#[tokio::test]
async fn a_failed_write_counts_the_rest_of_its_batch_and_the_queue() {
    let (metrics_tx, mut metrics_rx) = mpsc::channel::<MetricsEvent>(8);
    let (in_tx, _inbox) = channel::<ConnIn>(8);
    let (out_tx, out_rx) = channel::<FrameBatch>(4);
    // Queued before the pump starts: the writer takes the first batch,
    // writes one frame, fails on the second.
    out_tx.try_send(frames(3)).expect("room");
    out_tx.try_send(frames(2)).expect("room");
    let (read, write) = spawn_pumps(
        ConnectionId(61),
        futures::stream::pending::<std::io::Result<FrameBody>>(),
        FailsAfter { ok: 1, sent: 0 },
        in_tx,
        out_rx,
        PumpTimeouts::default(),
        Some(metrics_tx),
    );
    let t = transport_sample(&mut metrics_rx).await;
    assert_eq!(
        t.stream_frames_unwritten,
        2 + 2,
        "two of the first batch, the second whole"
    );
    assert_eq!(t.stream_batches_unwritten, 1, "the batch still queued");
    // The channel is closed: the sender learns it at once.
    assert!(
        out_tx.try_send(frames(1)).is_err(),
        "the outbound channel is closed"
    );
    tokio::time::timeout(Duration::from_secs(5), write)
        .await
        .expect("the writer ended")
        .expect("no panic");
    read.abort();
}

/// A write stall: the stalled frame, the rest of its batch and the queue.
#[tokio::test]
async fn a_write_stall_counts_the_rest_of_its_batch_and_the_queue() {
    let (metrics_tx, mut metrics_rx) = mpsc::channel::<MetricsEvent>(8);
    let (in_tx, _inbox) = channel::<ConnIn>(8);
    let (out_tx, out_rx) = channel::<FrameBatch>(4);
    out_tx.try_send(frames(2)).expect("room");
    out_tx.try_send(frames(1)).expect("room");
    out_tx.try_send(frames(4)).expect("room");
    let (read, _write) = spawn_pumps(
        ConnectionId(62),
        futures::stream::pending::<std::io::Result<FrameBody>>(),
        Wedged,
        in_tx,
        out_rx,
        PumpTimeouts {
            idle: None,
            write_stall: Some(Duration::from_millis(100)),
        },
        Some(metrics_tx),
    );
    let t = transport_sample(&mut metrics_rx).await;
    assert_eq!(t.stream_frames_unwritten, 2 + 1 + 4, "nothing was written");
    assert_eq!(
        t.stream_batches_unwritten, 2,
        "the two batches still queued"
    );
    read.abort();
}

/// An ordinary end (every sender gone) loses nothing and sends nothing.
#[tokio::test]
async fn an_ordinary_end_counts_nothing() {
    let (metrics_tx, mut metrics_rx) = mpsc::channel::<MetricsEvent>(8);
    let (in_tx, _inbox) = channel::<ConnIn>(8);
    let (out_tx, out_rx) = channel::<FrameBatch>(4);
    out_tx.try_send(frames(3)).expect("room");
    drop(out_tx);
    let (read, write) = spawn_pumps(
        ConnectionId(63),
        futures::stream::pending::<std::io::Result<FrameBody>>(),
        FailsAfter { ok: 3, sent: 0 },
        in_tx,
        out_rx,
        PumpTimeouts::default(),
        Some(metrics_tx),
    );
    tokio::time::timeout(Duration::from_secs(5), write)
        .await
        .expect("the writer ended")
        .expect("no panic");
    read.abort();
    assert!(metrics_rx.try_recv().is_err(), "nothing lost, nothing sent");
}

/// The frame a reader holds when the actor's inbox closes (the server
/// ended the session) is counted by its kind: here an RPC request, the
/// ledger's term.
#[tokio::test]
async fn the_readers_frame_refused_by_a_closed_inbox_is_counted_by_kind() {
    let (metrics_tx, mut metrics_rx) = mpsc::channel::<MetricsEvent>(8);
    // One slot: the heartbeat fills it, the request parks the reader.
    let (in_tx, mut inbox) = channel::<ConnIn>(1);
    let (_out_tx, out_rx) = channel::<FrameBatch>(4);
    let inbound = futures::stream::iter(vec![
        Ok(FrameBody::new(op::base::HEARTBEAT, Vec::new())),
        Ok::<_, std::io::Error>(FrameBody::new(op::base::RPC_REQ, vec![0; 8])),
    ])
    .chain(futures::stream::pending());
    let (read, write) = spawn_pumps(
        ConnectionId(64),
        inbound,
        FailsAfter { ok: 0, sent: 0 },
        in_tx,
        out_rx,
        PumpTimeouts::default(),
        Some(metrics_tx),
    );
    tokio::time::sleep(Duration::from_millis(50)).await;
    // The actor's end (B60): close, then drain what it holds.
    inbox.close();
    while inbox.try_recv().is_ok() {}
    let t = transport_sample(&mut metrics_rx).await;
    assert_eq!(t.stream_requests_dropped_closed, 1, "{t:?}");
    assert_eq!(t.stream_actions_dropped_closed, 0);
    assert_eq!(t.stream_control_frames_dropped_closed, 0);
    tokio::time::timeout(Duration::from_secs(5), read)
        .await
        .expect("the reader ended")
        .expect("no panic");
    write.abort();
}

/// A stall verdict with no reserved slot (the mailbox was full when the
/// pump was born) is delivered after the close — counted as deferred.
#[tokio::test]
async fn a_stall_verdict_without_a_reserved_slot_is_counted_as_deferred() {
    let (metrics_tx, mut metrics_rx) = mpsc::channel::<MetricsEvent>(8);
    let (in_tx, mut inbox) = channel::<ConnIn>(1);
    let hb = FrameBody::new(op::base::HEARTBEAT, Vec::new());
    in_tx.try_send(ConnIn::Frame(hb)).expect("room");
    let (out_tx, out_rx) = channel::<FrameBatch>(4);
    out_tx.try_send(frames(1)).expect("room");
    let (read, write) = spawn_pumps(
        ConnectionId(65),
        futures::stream::pending::<std::io::Result<FrameBody>>(),
        Wedged,
        in_tx,
        out_rx,
        PumpTimeouts {
            idle: None,
            write_stall: Some(Duration::from_millis(100)),
        },
        Some(metrics_tx),
    );
    let t = transport_sample(&mut metrics_rx).await;
    assert_eq!(t.writer_verdicts_deferred, 1, "{t:?}");
    assert_eq!(t.stream_frames_unwritten, 1);
    // Making room lets the late verdict in.
    let _ = inbox.recv().await;
    match tokio::time::timeout(Duration::from_secs(5), inbox.recv()).await {
        Ok(Some(ConnIn::ServerClosed { cause, .. })) => assert_eq!(cause, ServerClose::WriteStall),
        other => panic!("the late verdict: {other:?}"),
    }
    tokio::time::timeout(Duration::from_secs(5), write)
        .await
        .expect("the writer ended")
        .expect("no panic");
    read.abort();
}
