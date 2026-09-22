//! The length-prefix framing adapters (`[u32 LE length][u16 LE opcode]
//! [payload]`), shared by every stream transport.
//!
//! WHY one generic pair instead of per-transport copies: the framing is a
//! wire contract of the *protocol*, not of any transport. TCP's halves and
//! the rustls stream halves both implement `AsyncRead`/`AsyncWrite`, so one
//! generic reader (a [`Stream`] of decoded frames) and one generic writer
//! (a [`Sink`] of frames) serve both; the pump layer
//! ([`crate::pump::spawn_pumps`]) is already generic over exactly these two
//! shapes. The 4-byte prefix lives here and never leaks out.

use std::io;
use std::pin::Pin;
use std::task::Context;
use std::task::Poll;
use std::task::ready;

use bytes::Buf;
use bytes::BytesMut;
use futures::Sink;
use futures::Stream;
use futures::StreamExt;
use tokio::io::AsyncRead;
use tokio::io::AsyncWrite;
use tokio_util::codec::FramedRead;
use tokio_util::codec::LengthDelimitedCodec;

use gsb_protocol::FrameBody;

use crate::pump::WriteProgress;

/// A length-delimited codec configured the gsb way: little-endian prefix,
/// `max_frame_bytes` body ceiling (the transport-level guard).
pub(crate) fn codec(max_frame_bytes: usize) -> LengthDelimitedCodec {
    LengthDelimitedCodec::builder()
        .little_endian()
        .max_frame_length(max_frame_bytes)
        .new_codec()
}

/// Streaming view over any byte source; yields parsed frames.
pub(crate) struct FrameReader<S> {
    inner: FramedRead<S, LengthDelimitedCodec>,
}

impl<S: AsyncRead + Unpin> FrameReader<S> {
    pub(crate) fn new(source: S, max_frame_bytes: usize) -> Self {
        Self {
            inner: FramedRead::new(source, codec(max_frame_bytes)),
        }
    }
}

impl<S: AsyncRead + Unpin> Stream for FrameReader<S> {
    type Item = io::Result<FrameBody>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        match ready!(this.inner.poll_next_unpin(cx)) {
            None => Poll::Ready(None),
            Some(Err(e)) => Poll::Ready(Some(Err(e))),
            Some(Ok(chunk)) => Poll::Ready(Some(
                FrameBody::decode(chunk.freeze())
                    .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e)),
            )),
        }
    }
}

/// Sink view over any byte sink; appends `[u32 LE len][frame body]` per
/// frame and flushes through the underlying writer.
pub(crate) struct FrameWriter<S> {
    inner: S,
    buf: BytesMut,
    /// Bytes the underlying writer has accepted (the write-stall clock's
    /// signal — see [`WriteProgress`]).
    written: u64,
}

impl<S: AsyncWrite + Unpin> FrameWriter<S> {
    pub(crate) fn new(sink: S) -> Self {
        Self {
            inner: sink,
            buf: BytesMut::with_capacity(1024),
            written: 0,
        }
    }

    /// Write all queued bytes to the socket; Pending when the socket would
    /// block.
    fn drain(this: &mut Self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        loop {
            if this.buf.is_empty() {
                return Pin::new(&mut this.inner).poll_flush(cx);
            }
            let n = ready!(Pin::new(&mut this.inner).poll_write(cx, &this.buf))?;
            this.buf.advance(n);
            // Progress: the socket took `n` bytes of a frame that may be
            // far from done. (`n == 0` is not progress; the next
            // iteration surfaces it as the writer's own error or Pending.)
            this.written += n as u64;
        }
    }
}

/// The count moves on every `poll_write` that accepted bytes — for TCP the
/// kernel's socket buffer, for QUIC the stream's flow-control credit (the
/// peer's reads), for TLS rustls's bounded plaintext buffer. One residual
/// on TLS: once the frame buffer here is empty, the LAST ≤64 KiB rustls
/// still holds drain inside `poll_flush`, which reports no byte count;
/// a peer slower than that per window can still trip the clock at a
/// frame's tail.
impl<S> WriteProgress for FrameWriter<S> {
    fn bytes_written(&self) -> u64 {
        self.written
    }
}

impl<S: AsyncWrite + Unpin> Sink<FrameBody> for FrameWriter<S> {
    type Error = io::Error;

    fn poll_ready(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
        // Ready to accept more frames as soon as the queued bytes can be
        // written out (keeps the socket buffer from growing unbounded).
        let this = self.get_mut();
        Self::drain(this, cx)
    }

    fn start_send(self: Pin<&mut Self>, item: FrameBody) -> Result<(), io::Error> {
        let this = self.get_mut();
        let body = item.encode();
        // The 4-byte LE length prefix covers the frame body (opcode +
        // payload). Size limits are enforced by the reader's codec
        // (`max_frame_bytes`), so there is nothing to guard here.
        this.buf
            .extend_from_slice(&(body.len() as u32).to_le_bytes());
        this.buf.extend_from_slice(&body);
        Ok(())
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
        let this = self.get_mut();
        Self::drain(this, cx)
    }

    fn poll_close(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
        let this = self.get_mut();
        ready!(FrameWriter::<S>::drain(this, cx))?;
        Pin::new(&mut this.inner).poll_shutdown(cx)
    }
}
