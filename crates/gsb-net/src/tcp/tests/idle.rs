//! The reader pump's idle-timeout clock: the half-open TCP guardrail
//! (no FIN ever arrives), the window reset an active peer earns, and
//! the EOF case that must NOT be reported as an idle timeout.

use super::*;

use crate::pump::PumpTimeouts;

/// A peer that connects and says nothing: the reader must give up
/// after the idle window and tell the connection actor
/// (`ConnIn::ServerClosed`, idle reason).
#[tokio::test]
async fn idle_peer_gets_server_closed() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let (stream, _server) = connect(listener).await;
    let (reader, writer) = TcpReader::for_stream(stream, 1024);
    let (in_tx, mut in_rx) = channel::<ConnIn>(8);
    let (out_tx, out_rx) = channel::<FrameBatch>(8);
    let (read, write) = spawn_pumps(
        ConnectionId(1),
        reader,
        writer,
        in_tx,
        out_rx,
        PumpTimeouts {
            idle: Some(Duration::from_millis(200)),
            write_stall: None,
        },
        None,
    );
    let msg = tokio::time::timeout(Duration::from_secs(5), in_rx.recv())
        .await
        .expect("inbox open")
        .expect("pump notified");
    match msg {
        ConnIn::ServerClosed { cause, reason } => {
            assert_eq!(cause, gsb_core::conn::ServerClose::IdleTimeout);
            assert!(
                reason.contains("idle timeout"),
                "the idle reason must say why: {reason}"
            );
        }
        other => panic!("expected ServerClosed, got {other:?}"),
    }
    read.await.expect("reader pump exits");
    drop(out_tx);
    write.await.expect("writer pump exits");
}

/// A peer that keeps sending (any frame — a heartbeat, a move) resets
/// the window: no ServerClosed while the traffic flows; when it goes
/// quiet, the window fires. The "peer" is an mpsc stream (no TCP
/// buffering: frames arrive exactly when the test sends them, so the
/// pacing — and the reset semantics — are deterministic).
#[tokio::test]
async fn active_peer_resets_the_idle_window() {
    use std::io;
    use std::pin::Pin;
    use std::task::{Context, Poll};

    /// Writer stand-in: the test sends nothing outbound; the sink
    /// just accepts and discards.
    struct DiscardSink;
    impl Sink<FrameBody> for DiscardSink {
        type Error = io::Error;
        fn poll_ready(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
        fn start_send(self: Pin<&mut Self>, _item: FrameBody) -> io::Result<()> {
            Ok(())
        }
        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
        fn poll_close(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }
    // No socket, no bytes (and no stall clock armed below either).
    impl crate::pump::WriteProgress for DiscardSink {
        fn bytes_written(&self) -> u64 {
            0
        }
    }

    /// Stream view over the mpsc receiver (this tokio version's
    /// `Receiver` has no `Stream` impl): frames arrive exactly when
    /// the test sends them — deterministic pacing, no socket buffer.
    struct PeerStream {
        rx: tokio::sync::mpsc::Receiver<io::Result<FrameBody>>,
    }
    impl Stream for PeerStream {
        type Item = io::Result<FrameBody>;
        fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
            self.get_mut().rx.poll_recv(cx)
        }
    }

    // The controllable peer: one frame per test poke (100 ms cadence
    // against a 250 ms window).
    let (peer_tx, peer_rx) = tokio::sync::mpsc::channel::<io::Result<FrameBody>>(8);
    let (in_tx, mut in_rx) = channel::<ConnIn>(64);
    let (out_tx, out_rx) = channel::<FrameBatch>(8);
    let (read, write) = spawn_pumps(
        ConnectionId(2),
        PeerStream { rx: peer_rx }, // dropping peer_tx = EOF
        DiscardSink,
        in_tx,
        out_rx,
        PumpTimeouts {
            idle: Some(Duration::from_millis(250)),
            write_stall: None,
        },
        None,
    );

    // 100 ms cadence: the window (250 ms) can never elapse between
    // frames while the peer is active. `got` counts frames CUMULATIVELY
    // (try_recv consumes; a per-iteration counter would wait forever
    // for frames already drained on a previous poke).
    let mut got = 0u32;
    for i in 0..7u16 {
        peer_tx
            .send(Ok(FrameBody::new(9, b"hb".as_slice())))
            .await
            .unwrap();
        // Drain the frame notifications (the inbox would otherwise
        // fill and stall the pump's send) until poke `i` is accounted.
        while got <= i as u32 {
            match in_rx.try_recv() {
                Ok(ConnIn::Frame(_)) => got += 1,
                // An exit notification before all 7 pokes landed is a
                // bug (the window must reset on every frame).
                Ok(m) => panic!("unexpected early exit: {m:?}"),
                Err(_) => tokio::time::sleep(Duration::from_millis(10)).await,
            }
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(
        in_rx.try_recv().is_err(),
        "no exit notification right after the last frame"
    );
    // The peer goes quiet: the window must now fire.
    let msg = tokio::time::timeout(Duration::from_secs(3), in_rx.recv())
        .await
        .expect("inbox open")
        .expect("pump notified");
    match msg {
        ConnIn::ServerClosed { cause, reason } => {
            assert_eq!(cause, gsb_core::conn::ServerClose::IdleTimeout);
            assert!(reason.contains("idle timeout"), "reason: {reason}");
        }
        other => panic!("expected ServerClosed, got {other:?}"),
    }
    read.await.expect("reader pump exits");
    drop(out_tx);
    write.await.expect("writer pump exits");
}

/// A clean peer EOF is still reported as `Closed("peer closed")`, not
/// as an idle timeout: the deadline only fires while the read stays
/// pending, and a ready EOF always wins.
///
/// The idle window is far past the peer's close (10 s against 50 ms):
/// with a 200 ms window a process frozen from before the close to past
/// the window woke with both timers due, the FIN not yet seen by the IO
/// driver — an idle timeout the test did not mean (BACKLOG F34). A
/// reader that mistook the EOF for a quiet read still reports the idle
/// timeout, only later, and fails the match.
#[tokio::test]
async fn eof_reports_peer_closed_not_idle() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (peer, _) = listener.accept().await.unwrap();
        // Accept, stay silent for a moment, then close (EOF, no FIN
        // dance — the reader must see a clean end of stream).
        tokio::time::sleep(Duration::from_millis(50)).await;
        drop(peer);
    });
    let stream = TcpStream::connect(addr).await.unwrap();
    let (reader, writer) = TcpReader::for_stream(stream, 1024);
    let (in_tx, mut in_rx) = channel::<ConnIn>(8);
    let (out_tx, out_rx) = channel::<FrameBatch>(8);
    let (read, write) = spawn_pumps(
        ConnectionId(3),
        reader,
        writer,
        in_tx,
        out_rx,
        PumpTimeouts {
            idle: Some(Duration::from_secs(10)),
            write_stall: None,
        },
        None,
    );
    let msg = tokio::time::timeout(Duration::from_secs(30), in_rx.recv())
        .await
        .expect("inbox open")
        .expect("pump notified");
    match msg {
        ConnIn::Closed { reason } => assert_eq!(reason, "peer closed"),
        other => panic!("expected Closed(\"peer closed\"), got {other:?}"),
    }
    read.await.expect("reader pump exits");
    drop(out_tx);
    write.await.expect("writer pump exits");
    server.await.expect("one-shot server exits");
}
