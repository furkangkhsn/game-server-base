//! The writer pump's progress clock: the other half of the socket.
//!
//! The reader's idle window watches INBOUND silence. It cannot see a peer
//! that keeps its connection open, keeps its TCP stack alive, and simply
//! stops reading: that peer's receive window closes, the writer pump
//! parks inside its socket write, the outbound channel stays FULL (never
//! closed, so `w_closing` never fires), the room counts a dropped frame
//! every tick — and the session lives on holding its room slot and
//! registry row while receiving nothing.
//!
//! These tests force exactly that with a real loopback peer that accepts
//! the connection and never reads a byte, then feed the writer enough
//! frames to fill the kernel buffers. The bound is on PROGRESS, not age:
//! nothing written successfully for `write_stall` means the direction is
//! dead, and the verdict travels the actor's mailbox — an in-process
//! channel, never the socket, which is precisely the thing that is stuck.

use super::*;

use crate::pump::PumpTimeouts;

/// Bytes per test frame: big enough that a few dozen frames outrun any
/// loopback send/receive buffer pair, small enough to stay under the
/// reader-side frame ceiling nothing here parses anyway.
const CHUNK: usize = 64 * 1024;

/// A peer that accepts the connection and then never reads. Holding the
/// stream (rather than dropping it) is the whole point: no FIN, no RST,
/// the socket stays writable-in-principle and simply stops draining.
fn deaf_peer(listener: TcpListener) -> JoinHandle<()> {
    tokio::spawn(async move {
        // The binding is what keeps the socket open: no FIN, no RST.
        let (_peer, _) = listener.accept().await.expect("accept");
        // Park forever without reading. The task is aborted by the test.
        std::future::pending::<()>().await
    })
}

/// THE PROPERTY: a peer that never reads ends the session. The writer
/// pump notices that the socket has taken no byte for the whole window and
/// reports it to the connection actor over the actor's mailbox — the
/// ordinary `ConnIn::ServerClosed` teardown entry, the same one the
/// reader's idle window and the rUDP liveness bound use.
#[tokio::test]
async fn deaf_peer_ends_the_session() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let peer = deaf_peer(listener);
    let stream = TcpStream::connect(addr).await.unwrap();
    let (reader, writer) = TcpReader::for_stream(stream, CHUNK * 2);
    let (in_tx, mut in_rx) = channel::<ConnIn>(8);
    let (out_tx, out_rx) = channel::<FrameBatch>(4);
    let (read, write) = spawn_pumps(
        ConnectionId(11),
        reader,
        writer,
        in_tx,
        out_rx,
        PumpTimeouts {
            // No inbound clock at all: the write-stall bound must be the
            // ONLY mechanism that can end this session.
            idle: None,
            write_stall: Some(Duration::from_millis(300)),
        },
        None,
    );

    // Feed the pump until the socket wedges. `try_send` keeps the test
    // out of the parked-sender case: once the channel is full the writer
    // is demonstrably not draining it, which is the condition itself.
    let feeder = tokio::spawn(async move {
        let frame = FrameBody::new(7, vec![0u8; CHUNK]);
        loop {
            if out_tx.try_send(vec![frame.clone()]).is_err() {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }
    });

    let msg = tokio::time::timeout(Duration::from_secs(10), in_rx.recv())
        .await
        .expect(
            "the writer pump never reported the stalled socket: the session \
             is still holding its room slot and registry row while it can \
             receive nothing",
        )
        .expect("pump notified");
    match msg {
        ConnIn::ServerClosed { cause, reason } => {
            assert_eq!(cause, gsb_core::conn::ServerClose::WriteStall);
            assert!(
                reason.contains("write stall"),
                "the stall reason must say why: {reason}"
            )
        }
        other => panic!("expected ServerClosed, got {other:?}"),
    }

    // The teardown must not need the socket to accept anything: the
    // writer task ends on its own even though the peer is still deaf.
    tokio::time::timeout(Duration::from_secs(5), write)
        .await
        .expect("writer pump exits without waiting on the wedged socket")
        .expect("no panic");

    feeder.abort();
    read.abort();
    peer.abort();
}

/// The converse, so the fix cannot be "close whenever a write is slow":
/// a peer that is merely BEHIND — it drains, just lazily — keeps its
/// session. This is the "slow client is tolerated" contract: the clock
/// measures bytes the socket accepts, and a draining socket accepts them.
/// (`slow_reader` locks the harder half: a peer slower than one FRAME per
/// window.)
#[tokio::test]
async fn a_draining_peer_is_never_stalled() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    // A lazy but honest reader: 40 ms between reads, far longer than any
    // write takes, and it never stops.
    let peer = tokio::spawn(async move {
        let (mut peer, _) = listener.accept().await.expect("accept");
        let mut buf = vec![0u8; CHUNK];
        loop {
            tokio::time::sleep(Duration::from_millis(40)).await;
            match peer.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
        }
    });
    let stream = TcpStream::connect(addr).await.unwrap();
    let (reader, writer) = TcpReader::for_stream(stream, CHUNK * 2);
    let (in_tx, mut in_rx) = channel::<ConnIn>(8);
    let (out_tx, out_rx) = channel::<FrameBatch>(4);
    let (read, write) = spawn_pumps(
        ConnectionId(12),
        reader,
        writer,
        in_tx,
        out_rx,
        PumpTimeouts {
            idle: None,
            write_stall: Some(Duration::from_millis(300)),
        },
        None,
    );

    // A trickle whose GAPS are longer than the window (400 ms against
    // 300 ms), for several windows in a row. Two things must hold: each
    // write the socket takes restarts the clock, and waiting for work is not a
    // stall at all — a session with nothing to say for a while (a quiet
    // room, a low-Hz tick) is not a dead direction.
    for _ in 0..4u32 {
        out_tx
            .send(vec![FrameBody::new(7, vec![0u8; 512])])
            .await
            .expect("the writer pump is still draining the channel");
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert!(
            in_rx.try_recv().is_err(),
            "a peer that keeps draining must never be reported stalled"
        );
    }

    drop(out_tx);
    tokio::time::timeout(Duration::from_secs(5), write)
        .await
        .expect("writer pump exits when the channel closes")
        .expect("no panic");
    read.abort();
    peer.abort();
}

/// The verdict is in the actor's mailbox BEFORE the outbound channel
/// closes. The connection actor learns of a stall first as a failed send
/// — typically while parked on the very channel the stalled pump stopped
/// draining — and then looks in its mailbox for the reason
/// (`adopt_pending_close`); a verdict posted only AFTER the close could
/// be missed there, and the close would be counted as a bare dead
/// outbound path instead of the write stall it is.
///
/// The pump runs on a worker thread while this test spins on the
/// outbound channel's closed flag and reads the mailbox the instant it
/// flips. A pump that closes first and posts second leaves a window of
/// a microsecond or so, and the spin lands in it: with the two steps
/// swapped this test failed 10 runs out of 10 (a race cannot be forced
/// to lose every time, so that is a measured rate, not a proof).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_stall_verdict_is_posted_before_the_outbound_channel_closes() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let peer = deaf_peer(listener);
    let stream = TcpStream::connect(addr).await.unwrap();
    let (reader, writer) = TcpReader::for_stream(stream, CHUNK * 2);
    let (in_tx, mut in_rx) = channel::<ConnIn>(8);
    let (out_tx, out_rx) = channel::<FrameBatch>(4);
    let (read, write) = spawn_pumps(
        ConnectionId(13),
        reader,
        writer,
        in_tx,
        out_rx,
        PumpTimeouts {
            idle: None,
            write_stall: Some(Duration::from_millis(300)),
        },
        None,
    );

    // Fill the channel until the socket wedges, then SPIN (no await, no
    // yield) on the closed flag: the pump runs on a worker thread, so the
    // mailbox is read within nanoseconds of the close — the narrowest
    // window this side can observe. (A parked send would be woken only
    // after the pump had long finished both steps, so it could not tell
    // the two orders apart.)
    let frame = FrameBody::new(7, vec![0u8; CHUNK]);
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        match out_tx.try_send(vec![frame.clone()]) {
            Ok(()) => continue,
            Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {}
            Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => break,
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the stalled pump never closed its outbound channel"
        );
        std::hint::spin_loop();
    }
    match in_rx.try_recv() {
        Ok(ConnIn::ServerClosed { cause, .. }) => {
            assert_eq!(cause, gsb_core::conn::ServerClose::WriteStall)
        }
        other => panic!(
            "the outbound channel closed before the stall verdict was in \
             the mailbox: {other:?}"
        ),
    }

    let _ = tokio::time::timeout(Duration::from_secs(5), write).await;
    read.abort();
    peer.abort();
}
