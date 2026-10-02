//! The teardown close through the real pump and socket writer, over a
//! socket whose buffers are shrunk (as `slow_reader` does): the session
//! ends while the queue is full behind a peer that is not reading. A peer
//! that reads again gets every frame, then the 1001 — before B80 the
//! close was dropped there and the stream just ended. A peer that never
//! reads again costs the pump its stall window, and the abandoned close
//! is counted.

use super::*;

use tokio::net::TcpSocket;

/// A frame the tight socket cannot take in one go.
const FRAME: usize = 64 * 1024;
/// One frame in the socket writer's hands, the queue (64) full behind it.
const FRAMES: usize = 1 + 64;

/// A connected pair with both kernel buffers shrunk: `(server, peer)`.
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

/// The door's writer half over the tight socket, fed `FRAMES` frames and
/// then ended (the actor's outbound channel closes). Returns the peer
/// and the writer pump's handle.
async fn ended_behind_a_full_queue(
    stall: Option<Duration>,
    metrics: crate::TransportMetrics,
) -> (TcpStream, tokio::task::JoinHandle<()>) {
    let (ours, peer) = tight_pair().await;
    let (_read_half, write_half) = ours.into_split();
    let (tx, written) = spawn_socket_writer(write_half, metrics.clone());
    let closing = Arc::new(AtomicBool::new(false));
    let writer =
        WsWriter::new(tx, WsMessageMapping::GameEnvelope, closing, written).with_metrics(metrics);
    let (in_tx, _in_rx) = channel::<ConnIn>(8);
    let (out_tx, out_rx) = channel::<FrameBatch>(4);
    let (_read, write) = crate::pump::spawn_pumps(
        ConnectionId(81),
        futures::stream::pending::<io::Result<FrameBody>>(),
        writer,
        in_tx,
        out_rx,
        crate::pump::PumpTimeouts {
            idle: None,
            write_stall: stall,
        },
        None,
    );
    let frame = FrameBody::new(7, vec![0u8; FRAME]);
    for _ in 0..FRAMES {
        out_tx
            .send(vec![frame.clone()])
            .await
            .expect("the pump reads");
    }
    drop(out_tx);
    (peer, write)
}

/// Every server frame on the stream until its end: `(opcode, payload)`.
fn frames(mut bytes: &[u8]) -> Vec<(u8, Vec<u8>)> {
    let mut out = Vec::new();
    while !bytes.is_empty() {
        let opcode = bytes[0] & 0x0F;
        let (len, head) = match bytes[1] & 0x7F {
            126 => (u16::from_be_bytes([bytes[2], bytes[3]]) as usize, 4),
            127 => (
                u64::from_be_bytes(bytes[2..10].try_into().unwrap()) as usize,
                10,
            ),
            n => (n as usize, 2),
        };
        out.push((opcode, bytes[head..head + len].to_vec()));
        bytes = &bytes[head + len..];
    }
    out
}

/// The peer reads again: every frame, then the 1001 (the same bytes a
/// teardown close always carried), then the end of the stream.
#[tokio::test]
async fn a_peer_behind_a_full_queue_still_gets_the_going_away() {
    let (mut peer, write) = ended_behind_a_full_queue(None, None).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(
        !write.is_finished(),
        "the pump's close waits for a slot in the full queue"
    );
    let mut all = Vec::new();
    tokio::time::timeout(Duration::from_secs(20), peer.read_to_end(&mut all))
        .await
        .expect("the stream ends")
        .expect("read");
    let got = frames(&all);
    let games = got.iter().filter(|(op, _)| *op == OP_BIN).count();
    assert_eq!(games, FRAMES, "every frame");
    assert_eq!(
        got.last(),
        Some(&(OP_CLOSE, GOING_AWAY.to_vec())),
        "then the teardown close"
    );
    write.await.expect("the pump ends");
}

/// The peer never reads again: the pump gives its close up at the stall
/// window, and the abandoned close is counted.
#[tokio::test]
async fn a_close_the_stall_window_abandons_is_counted() {
    let (metrics, mut samples) = mpsc::channel(8);
    let (_peer, write) =
        ended_behind_a_full_queue(Some(Duration::from_millis(300)), Some(metrics)).await;
    tokio::time::timeout(Duration::from_secs(5), write)
        .await
        .expect("the pump gives up at its window")
        .expect("the pump ends");
    let t = sample(&mut samples).await;
    assert_eq!(t.ws_teardown_closes_unsent_stalled, 1, "{t:?}");
}
