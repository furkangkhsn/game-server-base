//! A reader-level rig for the RFC 6455 rules: a real loopback socket
//! feeds a bare [`WsReader`] (no pumps, no echo actor), and the control
//! replies it queues are read straight off its outbound queue. That makes
//! the exact close code of every rejection observable, and lets a test
//! tell "delivered", "failed" and "ended" apart without a peer in between.

use super::*;
use futures::StreamExt;
use std::sync::atomic::AtomicBool;
use tokio::net::TcpListener;
use tokio::net::tcp::OwnedWriteHalf;
use tokio::sync::mpsc;

/// What the reader pushed onto the socket-writer's queue, in order.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Queued {
    Control(u8, Vec<u8>),
    Game,
    Shutdown,
}

pub(super) struct ReaderRig {
    reader: WsReader,
    queue: mpsc::Receiver<WsOut>,
    client: TcpStream,
    masks: MaskGen,
    /// Held so the server side of the socket stays whole for the test.
    _server_write: OwnedWriteHalf,
}

impl ReaderRig {
    pub(super) async fn new() -> Self {
        Self::with_max(DEFAULT_MAX_MESSAGE_BYTES).await
    }

    pub(super) async fn with_max(max_message_bytes: usize) -> Self {
        Self::with(max_message_bytes, WsMessageMapping::GameEnvelope).await
    }

    pub(super) async fn with(max_message_bytes: usize, mapping: WsMessageMapping) -> Self {
        Self::build(max_message_bytes, mapping, OUT_QUEUE_CAPACITY, None).await
    }

    /// A reader whose control queue holds `queue` items and whose losses
    /// go to `metrics` (B58).
    pub(super) async fn with_queue(queue: usize, metrics: crate::TransportMetrics) -> Self {
        Self::build(
            DEFAULT_MAX_MESSAGE_BYTES,
            WsMessageMapping::GameEnvelope,
            queue,
            metrics,
        )
        .await
    }

    async fn build(
        max_message_bytes: usize,
        mapping: WsMessageMapping,
        queue: usize,
        metrics: crate::TransportMetrics,
    ) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        let client = TcpStream::connect(addr).await.expect("connect");
        let (server, _) = listener.accept().await.expect("accept");
        let (read_half, write_half) = server.into_split();
        let (tx, queue) = mpsc::channel(queue);
        let reader = WsReader::new(
            read_half,
            max_message_bytes,
            mapping,
            tx,
            Arc::new(AtomicBool::new(false)),
            metrics,
        );
        Self {
            reader,
            queue,
            client,
            masks: MaskGen(7),
            _server_write: write_half,
        }
    }

    /// One masked client frame.
    pub(super) async fn send(&mut self, fin: bool, opcode: u8, payload: &[u8]) {
        let frame = encode_client_frame(fin, opcode, payload, self.masks.next(), true);
        self.send_raw(&frame).await;
    }

    pub(super) async fn send_raw(&mut self, bytes: &[u8]) {
        self.client.write_all(bytes).await.expect("client write");
    }

    /// The reader's next item, bounded so a hang fails instead of stalling.
    pub(super) async fn next(&mut self) -> Option<io::Result<FrameBody>> {
        tokio::time::timeout(Duration::from_secs(5), self.reader.next())
            .await
            .expect("the reader neither yielded nor failed within 5 s")
    }

    /// The next item must be a delivered game frame.
    pub(super) async fn game(&mut self) -> FrameBody {
        match self.next().await {
            Some(Ok(frame)) => frame,
            Some(Err(e)) => panic!("expected a game frame, the reader failed: {e}"),
            None => panic!("expected a game frame, the stream ended"),
        }
    }

    /// The reader must FAIL the connection: the pump gets `InvalidData`,
    /// and exactly one close frame is queued. Returns its status code.
    pub(super) async fn failure_code(&mut self) -> u16 {
        let err = match self.next().await {
            Some(Err(e)) => e,
            Some(Ok(frame)) => panic!("delivered a frame (op {}) instead of failing", frame.op),
            None => panic!("the stream ended cleanly instead of failing"),
        };
        assert_eq!(err.kind(), io::ErrorKind::InvalidData, "{err}");
        match self.drain().as_slice() {
            [Queued::Control(OP_CLOSE, payload)] => match payload.as_slice() {
                [hi, lo] => u16::from_be_bytes([*hi, *lo]),
                other => panic!("a failure close carries exactly a code, got {other:?}"),
            },
            other => panic!("expected exactly one queued close, got {other:?}"),
        }
    }

    /// Drop the reader (as its pump does when the connection ends).
    pub(super) fn end(self) {
        drop(self.reader);
    }

    /// Everything the reader has queued so far.
    pub(super) fn drain(&mut self) -> Vec<Queued> {
        let mut out = Vec::new();
        while let Ok(item) = self.queue.try_recv() {
            out.push(match item {
                WsOut::Control(op, payload) => Queued::Control(op, payload),
                WsOut::Game(_) => Queued::Game,
                WsOut::Shutdown => Queued::Shutdown,
            });
        }
        out
    }
}
