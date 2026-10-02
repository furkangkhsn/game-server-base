//! The QUIC send half the shared framing writes into: quinn's
//! `SendStream`, with a shutdown that waits for the peer's
//! acknowledgement instead of returning at `finish`.
//!
//! WHY (the close notice, docs/DESIGN.md §5.6): a QUIC connection ends
//! when its last stream handle is dropped, and that implicit close is
//! IMMEDIATE — stream data still in flight is abandoned. The writer pump
//! closes its sink and drops it right after a session's last frame,
//! which on every server-initiated end IS the close notice (`ERROR` code
//! 9 or 14); when the reader pump has already let go of its half (a
//! refused stream, or a client that kept sending after the actor ended),
//! that drop was the last handle and the notice died with it. A TCP
//! socket keeps delivering queued bytes after `shutdown`; this makes
//! the QUIC door do the same.
//!
//! Bounded: the pump runs `close` under its write-stall window (no byte
//! moves while waiting, so the window cuts a wait on a peer that never
//! acknowledges), and without that window quinn's own idle timeout ends
//! the connection, which resolves the wait with an error.
//!
//! It also carries the connection's path feed (B103, `super::path`):
//! the send half is where bytes move, so a write that took bytes is the
//! moment a due sample of quinn's statistics is read.

use std::future::Future;
use std::io;
use std::pin::Pin;
use std::task::{Context, Poll, ready};

use tokio::io::AsyncWrite;

/// The acknowledgement wait `SendStream::stopped` returns (it owns a
/// handle on the connection, which is what keeps it open meanwhile).
type Acked =
    Pin<Box<dyn Future<Output = Result<Option<quinn::VarInt>, quinn::StoppedError>> + Send + Sync>>;

pub(crate) struct QuicSend {
    stream: quinn::SendStream,
    /// Set once the stream is finished: resolves when the peer has
    /// acknowledged every byte, stopped the stream, or the connection
    /// died — any of which ends the shutdown.
    acked: Option<Acked>,
    /// The server side's path feed (`None` on a client's stream).
    path: Option<super::path::PathFeed>,
}

impl QuicSend {
    pub(crate) fn new(stream: quinn::SendStream) -> Self {
        Self {
            stream,
            acked: None,
            path: None,
        }
    }

    /// Feed the connection's path to its actor as bytes are written.
    pub(crate) fn with_path(mut self, feed: super::path::PathFeed) -> Self {
        self.path = Some(feed);
        self
    }
}

impl AsyncWrite for QuicSend {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        let wrote = ready!(AsyncWrite::poll_write(Pin::new(&mut this.stream), cx, buf));
        if let (Ok(1..), Some(feed)) = (&wrote, this.path.as_mut()) {
            feed.wrote();
        }
        Poll::Ready(wrote)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        AsyncWrite::poll_flush(Pin::new(&mut self.get_mut().stream), cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        if this.acked.is_none() {
            ready!(AsyncWrite::poll_shutdown(Pin::new(&mut this.stream), cx))?;
            this.acked = Some(Box::pin(this.stream.stopped()));
        }
        if let Some(acked) = this.acked.as_mut() {
            // The outcome does not matter: acknowledged, stopped by the
            // peer, or a dead connection all mean there is nothing left
            // to wait for.
            let _ = ready!(acked.as_mut().poll(cx));
        }
        Poll::Ready(Ok(()))
    }
}
