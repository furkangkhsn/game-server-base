//! The write-stall clock over the WebSocket door: bytes, not frames.
//!
//! The WS door differs from the other stream doors in one way that
//! matters here: the pump does not own the socket. It hands each frame to
//! a bounded queue drained by the door's single socket-writer task, so
//! the pump's own `send` completes when a QUEUE SLOT frees — i.e. when
//! the writer task has finished writing a WHOLE earlier frame. A progress
//! clock that only watched the pump's sends would therefore still count
//! frames here, however the pump itself counted. The byte signal has to
//! come from the task that actually writes the socket.
//!
//! Both halves are locked: a slow but steady reader survives frames that
//! each take several windows to drain, and a reader that stops entirely
//! still dies (the socket-writer's byte count must not move on its own).

use super::*;

use std::sync::atomic::AtomicBool;

use tokio::net::TcpSocket;

/// A whole second: the reader's pace (below) must stay far from the
/// window even when the machine stalls the test process (BACKLOG F25).
const WINDOW: Duration = Duration::from_secs(1);
/// Each frame takes ≥ 1 s (128 reads of at most [`READ_CHUNK`], each
/// after a [`READ_EVERY`] sleep) — longer than the window — to drain.
const FRAME: usize = 128 * 1024;
/// How long the slow reader reads: a few windows.
const READ_FOR: Duration = Duration::from_secs(4);
/// Enough frames to fill the door's queue (64) with the socket behind it
/// wedged, so the pump itself has to wait for a queue slot.
const FRAMES: usize = 70;
const READ_CHUNK: usize = 1024;
const READ_EVERY: Duration = Duration::from_millis(8);

/// A connected pair with both kernel buffers shrunk (explicit sizes also
/// switch off autotuning): the kernel wakes a blocked writer only once
/// about a quarter of its send buffer is free again (measured, SECURITY
/// §3.5 row 6, BACKLOG B15b), so with an autotuned
/// multi-megabyte send buffer a slow reader would surface as megabyte
/// bursts seconds apart — a statement about the kernel, not about the
/// clock under test.
async fn tight_pair() -> (TcpStream, TcpStream) {
    let lsock = TcpSocket::new_v4().unwrap();
    lsock.set_recv_buffer_size(4096).unwrap();
    lsock.bind("127.0.0.1:0".parse().unwrap()).unwrap();
    let listener = lsock.listen(1).unwrap();
    let addr = listener.local_addr().unwrap();
    let csock = TcpSocket::new_v4().unwrap();
    csock.set_send_buffer_size(4096).unwrap();
    let (ours, theirs) = tokio::join!(csock.connect(addr), listener.accept());
    (ours.unwrap(), theirs.unwrap().0)
}

/// THE REGRESSION LOCK on the WS door, below the HTTP upgrade (which
/// has no bearing on the write path, and whose accepted socket cannot
/// have its buffers shrunk from here): the door's own socket-writer task
/// and pump-facing writer, built by the same `spawn_socket_writer` the
/// transport uses, over a tight socket. The peer reads ~125 KB/s —
/// never stopping — while each frame takes over a window to drain and the pump
/// sits waiting on a full queue.
#[tokio::test]
async fn a_slow_but_steady_ws_reader_survives_frames_longer_than_the_window() {
    let (ours, mut peer) = tight_pair().await;
    let (_read_half, write_half) = ours.into_split();
    let (tx, written) = spawn_socket_writer(write_half, None);
    let writer = WsWriter::new(
        tx,
        WsMessageMapping::GameEnvelope,
        Arc::new(AtomicBool::new(false)),
        written,
    );
    let (in_tx, mut in_rx) = channel::<ConnIn>(8);
    let (out_tx, out_rx) = channel::<FrameBatch>(4);
    let (read, _write) = crate::pump::spawn_pumps(
        ConnectionId(51),
        futures::stream::pending::<io::Result<FrameBody>>(),
        writer,
        in_tx,
        out_rx,
        crate::pump::PumpTimeouts {
            idle: None,
            write_stall: Some(WINDOW),
        },
        None,
    );
    let _feeder = tokio::spawn(async move {
        let frame = FrameBody::new(7, vec![0u8; FRAME]);
        for _ in 0..FRAMES {
            if out_tx.send(vec![frame.clone()]).await.is_err() {
                return;
            }
        }
        // Keep the channel open: its close would end the session itself.
        std::future::pending::<()>().await
    });

    let started = std::time::Instant::now();
    let mut got = 0usize;
    let mut buf = vec![0u8; READ_CHUNK];
    while started.elapsed() < READ_FOR {
        tokio::time::sleep(READ_EVERY).await;
        match tokio::time::timeout(Duration::from_secs(5), peer.read(&mut buf)).await {
            Ok(Ok(0)) | Ok(Err(_)) => panic!(
                "the server ended the session after {got} bytes ({:?} in) \
                 while this peer was reading: {:?}",
                started.elapsed(),
                in_rx.try_recv()
            ),
            Ok(Ok(n)) => got += n,
            Err(_) => panic!("the server stopped writing after {got} bytes"),
        }
    }
    assert!(
        got < FRAME * 6,
        "not vacuous: {got} bytes in {:?} means frames drained faster than \
         the window — the buffers were not tight",
        READ_FOR
    );
    assert!(
        in_rx.try_recv().is_err(),
        "no close may be reported for a peer that kept reading"
    );
    read.abort();
}

/// A deaf WS peer over the REAL door (upgrade, accept, the transport's
/// own wiring): feeds until the queue and the socket behind it are full.
async fn wedged_session(conn: u64) -> (TcpStream, tokio::sync::mpsc::Receiver<ConnIn>) {
    let transport: Arc<dyn Transport> = Arc::new(WsTransport::default());
    let listener = transport
        .bind(SocketAddr::from(([127, 0, 0, 1], 0)))
        .await
        .expect("bind");
    let addr = listener.local_addr().unwrap();
    let client = tokio::spawn(async move {
        let sock = TcpSocket::new_v4().unwrap();
        sock.set_recv_buffer_size(4096).unwrap();
        let stream = sock.connect(addr).await.expect("connect");
        FakeWsClient::handshake(stream, addr).await.into_stream()
    });
    let endpoint = listener.accept().await.expect("ws accept");
    let (in_tx, in_rx) = channel::<ConnIn>(8);
    let (out_tx, out_rx) = channel::<FrameBatch>(4);
    let (_read, _write) = endpoint.start_pump(
        ConnectionId(conn),
        in_tx,
        out_rx,
        crate::pump::PumpTimeouts {
            idle: None,
            write_stall: Some(WINDOW),
        },
    );
    let stream = client.await.expect("client handshake");
    tokio::spawn(async move {
        // The accepted socket's send buffer autotunes (up to 4 MiB here),
        // so it takes a lot to wedge: 64 queued frames plus the socket.
        let frame = FrameBody::new(7, vec![0u8; 256 * 1024]);
        for _ in 0..100 {
            if out_tx.send(vec![frame.clone()]).await.is_err() {
                return;
            }
        }
        std::future::pending::<()>().await
    });
    (stream, in_rx)
}

/// The other half: a WS peer that reads NOTHING still dies inside a few
/// windows — the socket-writer's byte count must not move by itself.
#[tokio::test]
async fn a_deaf_ws_peer_still_dies() {
    let (_stream, mut in_rx) = wedged_session(52).await;
    let msg = tokio::time::timeout(Duration::from_secs(20), in_rx.recv())
        .await
        .expect("a deaf WS peer must be ended by the write-stall clock")
        .expect("inbox open");
    match msg {
        ConnIn::ServerClosed { cause, .. } => {
            assert_eq!(cause, gsb_core::conn::ServerClose::WriteStall)
        }
        other => panic!("expected the write-stall verdict, got {other:?}"),
    }
}

/// The door says WHEN its socket last took a byte (BACKLOG B15c): the
/// pump sees the socket-writer's count only when it looks — at its
/// deadline, while it waits on a full queue — and restarts the window at
/// this moment, not at the look. Without it a verdict could come up to
/// two windows after the last byte (`pump::writer::tests` locks the
/// pump's half).
#[tokio::test(start_paused = true)]
async fn the_ws_writer_says_when_its_socket_last_took_a_byte() {
    use crate::pump::WriteProgress;
    let (ours, _peer) = tight_pair().await;
    let (_read_half, write_half) = ours.into_split();
    let (tx, written) = spawn_socket_writer(write_half, None);
    let writer = WsWriter::new(
        tx.clone(),
        WsMessageMapping::GameEnvelope,
        Arc::new(AtomicBool::new(false)),
        written,
    );
    let at = tokio::time::Instant::now() + Duration::from_secs(3);
    tokio::time::sleep_until(at).await;
    tx.send(WsOut::Game(Bytes::from_static(&[0, 0, 0, 2, 7, 0])))
        .await
        .expect("the socket writer is running");
    for _ in 0..1000 {
        if writer.bytes_written() > 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    assert!(
        writer.bytes_written() > 0,
        "the socket writer wrote the frame"
    );
    let last = writer.last_write_at().expect("the WS door says when");
    assert!(
        last >= at && last <= at + Duration::from_millis(50),
        "the frame went out at {at:?}; the door says {last:?}"
    );
}
