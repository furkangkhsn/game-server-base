//! The WebSocket door's outbound queue must WAKE the pump.
//!
//! The pump hands frames to a bounded queue drained by the door's
//! socket-writer task. When the queue is full the pump's send parks in
//! the writer's `poll_ready` — and something has to wake it when a slot
//! frees. The writer used to build a fresh `reserve_owned` future on
//! every poll and drop it when it returned `Pending`, which removes the
//! waiter from the channel's wait list: the wake-up was lost. With the
//! stall clock off the pump then slept forever; with it on, only the
//! clock's deadline ever woke it — which, before the clock counted
//! bytes, killed any WS session whose 64-frame queue was full for one
//! window however fast the socket drained.

use super::*;

const FRAME: usize = 256 * 1024;
/// Enough to fill the queue (64 frames) and the socket behind it (whose
/// send buffer autotunes up to 4 MiB on loopback).
const FRAMES: usize = 100;

/// The door's queue must WAKE the pump when a slot frees. With no stall
/// clock at all (so nothing else can wake it), a pump that found the
/// queue full must resume as soon as the socket-writer drains — here the
/// peer first reads nothing until everything is wedged, then reads as
/// fast as it can, and every byte of every frame must arrive.
#[tokio::test]
async fn a_full_ws_queue_wakes_the_pump_when_it_drains() {
    let transport: Arc<dyn Transport> = Arc::new(WsTransport::default());
    let listener = transport
        .bind(SocketAddr::from(([127, 0, 0, 1], 0)))
        .await
        .expect("bind");
    let addr = listener.local_addr().unwrap();
    let client = tokio::spawn(async move { FakeWsClient::connect(addr).await.into_stream() });
    let endpoint = listener.accept().await.expect("ws accept");
    let (in_tx, _in_rx) = channel::<ConnIn>(8);
    let (out_tx, out_rx) = channel::<FrameBatch>(4);
    let (_read, _write) = endpoint.start_pump(
        ConnectionId(53),
        in_tx,
        out_rx,
        crate::pump::PumpTimeouts::default(),
    );
    let mut stream = client.await.expect("client handshake");
    let feeder = tokio::spawn(async move {
        let frame = FrameBody::new(7, vec![0u8; FRAME]);
        for _ in 0..FRAMES {
            out_tx.send(vec![frame.clone()]).await.expect("pump alive");
        }
        out_tx
    });
    // Let everything wedge: queue full, socket full, pump parked.
    tokio::time::sleep(Duration::from_millis(500)).await;
    // Each frame on the wire: WS header (10 bytes for a 64-bit length)
    // + the game envelope (4-byte length, 2-byte opcode, payload).
    let want = FRAMES * (10 + 4 + 2 + FRAME);
    let mut got = 0usize;
    let mut buf = vec![0u8; 256 * 1024];
    while got < want {
        match tokio::time::timeout(Duration::from_secs(5), stream.read(&mut buf)).await {
            Ok(Ok(n)) if n > 0 => got += n,
            other => panic!(
                "the pump never resumed after the queue drained: {got} of \
                 {want} bytes, then {other:?}"
            ),
        }
    }
    let _out_tx = feeder.await.expect("every frame was taken by the pump");
}
