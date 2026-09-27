//! The write-stall clock measures BYTES, not frames.
//!
//! The bound exists for the peer that has stopped reading. A peer that
//! reads steadily but slowly is not that peer — it is merely behind, and
//! "slow client is tolerated" is the contract. The clock used to restart
//! only when a whole frame's send (and flush) COMPLETED, so a frame that
//! takes longer than the window to drain killed a peer that was reading
//! all along: as frames grow, a progress bound quietly turns into an age
//! bound (the 10k measurement: ~80 KB snapshots, ~1.2 MB/s per
//! connection, 4486 sessions killed for "nothing written for 10s").
//!
//! This forces exactly that on a real TCP socket: both kernel buffers are
//! shrunk so the peer's read rate — not buffer headroom — paces the
//! writer, and ONE frame takes several windows to drain.

use super::*;

use crate::pump::PumpTimeouts;
use tokio::net::TcpSocket;

/// The stall window under test.
const WINDOW: Duration = Duration::from_millis(300);
/// One frame: ~2 s at the peer's read rate below, i.e. ~7 windows.
const FRAME: usize = 256 * 1024;
/// The peer's pace: at most this many bytes per read, one read per tick.
const READ_CHUNK: usize = 1024;
const READ_EVERY: Duration = Duration::from_millis(8);

/// A connected pair with both kernel buffers shrunk (explicit sizes also
/// switch off the kernel's buffer autotuning), so what the peer has not
/// read cannot hide in megabytes of socket buffer.
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

/// THE REGRESSION LOCK: a peer that reads slowly but continuously keeps
/// its session although one frame takes many windows to drain — and
/// receives the whole frame.
#[tokio::test]
async fn a_slow_but_steady_reader_survives_a_frame_longer_than_the_window() {
    let (ours, mut peer) = tight_pair().await;
    let (reader, writer) = TcpReader::for_stream(ours, 64);
    let (in_tx, mut in_rx) = channel::<ConnIn>(8);
    let (out_tx, out_rx) = channel::<FrameBatch>(4);
    let (read, write) = spawn_pumps(
        ConnectionId(31),
        reader,
        writer,
        in_tx,
        out_rx,
        PumpTimeouts {
            idle: None,
            write_stall: Some(WINDOW),
        },
        None,
    );
    out_tx
        .send(vec![FrameBody::new(7, vec![0u8; FRAME])])
        .await
        .expect("the writer pump takes the frame");

    // Read at the pace above until the whole frame (4-byte length prefix
    // + 2-byte opcode + payload) is in.
    let want = 4 + 2 + FRAME;
    let started = std::time::Instant::now();
    let mut got = 0usize;
    let mut buf = vec![0u8; READ_CHUNK];
    while got < want {
        tokio::time::sleep(READ_EVERY).await;
        match tokio::time::timeout(Duration::from_secs(5), peer.read(&mut buf)).await {
            Ok(Ok(0)) | Ok(Err(_)) => panic!(
                "the server ended the session after {got} of {want} bytes \
                 ({:?} in), while this peer was reading: {:?}",
                started.elapsed(),
                in_rx.try_recv()
            ),
            Ok(Ok(n)) => got += n,
            Err(_) => panic!("the writer stopped writing after {got} of {want} bytes"),
        }
    }
    let took = started.elapsed();

    // Not vacuous: the frame really did take several windows to drain
    // (if the buffers had absorbed it, this would prove nothing).
    assert!(
        took >= WINDOW * 3,
        "the frame drained in {took:?}; it must take several {WINDOW:?} \
         windows for this test to mean anything"
    );
    assert!(
        in_rx.try_recv().is_err(),
        "no close may have been reported for a peer that kept reading"
    );

    drop(out_tx);
    tokio::time::timeout(Duration::from_secs(5), write)
        .await
        .expect("writer pump exits when the channel closes")
        .expect("no panic");
    read.abort();
}
