//! The read half: WS frames in, game frames out. The per-frame parse
//! and the message-level dispatch live in child modules so they still
//! reach this struct's private reassembly state.

use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::task::Context;
use std::task::Poll;

use bytes::BytesMut;
use futures::Stream;
use tokio::io::AsyncRead;
use tokio::io::ReadBuf;
use tokio::net::tcp::OwnedReadHalf;
use tokio::sync::mpsc;

use gsb_protocol::FrameBody;

use crate::ws::*;

mod dispatch;
mod flush;
mod parse;

/// What one [`WsReader::step`] made of the buffered bytes.
pub(super) enum Step {
    /// Need more bytes off the socket before another frame can be parsed.
    NeedData,
    /// Progress was made (pong sent, fragment accumulated); keep stepping.
    Continue,
    /// One game frame is ready for the pump.
    Yield(FrameBody),
    /// The peer initiated the close handshake (echo already queued).
    Done,
}

/// The pump-facing reader: parses WebSocket frames off the socket's read
/// half and yields game frames. Also generates control replies (pongs,
/// close echoes, protocol-failure closes) by pushing them onto the shared
/// outbound queue — see the module docs for why that queue exists.
pub(super) struct WsReader {
    sock: OwnedReadHalf,
    scratch: Vec<u8>,
    buf: BytesMut,
    max_message_bytes: usize,
    mapping: WsMessageMapping,
    ctrl: mpsc::Sender<WsOut>,
    /// Set as soon as this connection has queued a close frame, so the
    /// writer pump's teardown never emits a second one.
    closing: Arc<AtomicBool>,
    /// Opcode of the data message being reassembled (`None` = none).
    frag_opcode: Option<u8>,
    frag_data: BytesMut,
    /// Close frames and pongs dropped on the full control queue (B58),
    /// and on the closed one (B83), and their path to the collector (see
    /// `flush`).
    close_frames_dropped: u64,
    pongs_dropped: u64,
    close_frames_dropped_closed: u64,
    pongs_dropped_closed: u64,
    flusher: crate::metrics::Flusher,
}

impl WsReader {
    pub(super) fn new(
        sock: OwnedReadHalf,
        max_message_bytes: usize,
        mapping: WsMessageMapping,
        ctrl: mpsc::Sender<WsOut>,
        closing: Arc<AtomicBool>,
        metrics: crate::TransportMetrics,
    ) -> Self {
        Self {
            sock,
            scratch: vec![0u8; READ_CHUNK],
            buf: BytesMut::with_capacity(READ_CHUNK),
            max_message_bytes,
            mapping,
            ctrl,
            closing,
            frag_opcode: None,
            frag_data: BytesMut::new(),
            close_frames_dropped: 0,
            pongs_dropped: 0,
            close_frames_dropped_closed: 0,
            pongs_dropped_closed: 0,
            flusher: crate::metrics::Flusher::new(metrics),
        }
    }

    /// Fail the WebSocket connection: queue a close frame carrying `code`
    /// (best effort — poll context means `try_send`; a full queue drops the
    /// notice, teardown follows regardless), then hand the pump an error.
    pub(super) fn proto_fail(&mut self, code: u16, why: impl std::fmt::Display) -> io::Error {
        self.closing.store(true, Ordering::SeqCst);
        self.queue_control(OP_CLOSE, code.to_be_bytes().to_vec());
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("websocket protocol violation ({code}): {why}"),
        )
    }
    /// Pull one read's worth of bytes into `buf`; `Ok(false)` = clean EOF
    /// at a frame boundary (stream over), `Err` = truncated mid-frame.
    pub(super) fn fill(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<bool>> {
        let n = {
            let mut rb = ReadBuf::new(&mut self.scratch);
            match Pin::new(&mut self.sock).poll_read(cx, &mut rb) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                Poll::Ready(Ok(())) => rb.filled().len(),
            }
        };
        if n == 0 {
            let mid_frame =
                !self.buf.is_empty() || self.frag_opcode.is_some() || !self.frag_data.is_empty();
            if mid_frame {
                return Poll::Ready(Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "connection closed mid-frame",
                )));
            }
            return Poll::Ready(Ok(false));
        }
        // Disjoint field borrows; one copy keeps `buf` appendable without
        // any unsafe zeroing dance around `ReadBuf::uninit`.
        self.buf.extend_from_slice(&self.scratch[..n]);
        Poll::Ready(Ok(true))
    }
}

impl Stream for WsReader {
    type Item = io::Result<FrameBody>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        loop {
            match this.step() {
                Ok(Step::NeedData) => {}
                Ok(Step::Continue) => continue,
                Ok(Step::Yield(frame)) => return Poll::Ready(Some(Ok(frame))),
                Ok(Step::Done) => return Poll::Ready(None),
                Err(e) => return Poll::Ready(Some(Err(e))),
            }
            match this.fill(cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(Ok(true)) => {}
                Poll::Ready(Ok(false)) => return Poll::Ready(None),
                Poll::Ready(Err(e)) => return Poll::Ready(Some(Err(e))),
            }
        }
    }
}
