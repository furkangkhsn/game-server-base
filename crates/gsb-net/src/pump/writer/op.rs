//! One awaited sink operation that keeps the sink REACHABLE while it is
//! pending, so the stall clock can read the transport's byte count in
//! the middle of a send. A child of the writer pump.
//!
//! Why not `SinkExt::send`: its future holds the only `&mut` to the sink
//! for as long as it is pending, so nothing else can look at the sink
//! until the WHOLE frame is out — which is exactly the frame-granular
//! clock this replaces. `Op` holds the same `&mut` and does the same
//! three steps (`poll_ready`, `start_send`, `poll_flush`), but also reads
//! [`WriteProgress::bytes_written`] through that borrow: on every poll
//! (the pump is polled when the socket turns writable, i.e. exactly when
//! bytes flow), and whenever the stall deadline fires. No lock, no
//! shared cell, no second task — the same `&mut` the send already has.

use std::future::Future;
use std::io;
use std::pin::Pin;
use std::task::{Context, Poll, ready};
use tokio::time::Instant;

use futures::Sink;

use gsb_protocol::FrameBody;

use crate::pump::WriteProgress;

/// Which sink operation an [`Op`] drives.
#[derive(Debug, Clone, Copy)]
enum Kind {
    /// `poll_ready` → `start_send` → `poll_flush` (what `SinkExt::send`
    /// does).
    Send,
    Flush,
    Close,
}

/// One pending sink operation plus the last time the transport was seen
/// to accept a byte.
pub(super) struct Op<'a, W> {
    sink: &'a mut W,
    /// The frame, until `start_send` takes it.
    item: Option<FrameBody>,
    kind: Kind,
    /// The transport's byte count when last read.
    seen: u64,
    /// When `seen` last moved — or the window's start, if it has not.
    moved_at: Instant,
}

impl<'a, W> Op<'a, W>
where
    W: Sink<FrameBody, Error = io::Error> + WriteProgress + Unpin,
{
    fn new(sink: &'a mut W, item: Option<FrameBody>, kind: Kind, since: Instant) -> Self {
        let seen = sink.bytes_written();
        Self {
            sink,
            item,
            kind,
            seen,
            moved_at: since,
        }
    }

    /// Send one frame (and flush it). `since`: the window's start.
    pub(super) fn send(sink: &'a mut W, frame: FrameBody, since: Instant) -> Self {
        Self::new(sink, Some(frame), Kind::Send, since)
    }

    pub(super) fn flush(sink: &'a mut W, since: Instant) -> Self {
        Self::new(sink, None, Kind::Flush, since)
    }

    pub(super) fn close(sink: &'a mut W, since: Instant) -> Self {
        Self::new(sink, None, Kind::Close, since)
    }

    /// Read the byte count; a change restarts the window at the moment
    /// the transport took the byte — when it can say (a socket written by
    /// another task, seen only at a deadline), else now. Returns when the
    /// transport last moved (or the window's start).
    pub(super) fn observe(&mut self) -> Instant {
        let n = self.sink.bytes_written();
        if n != self.seen {
            self.seen = n;
            let at = self.sink.last_write_at().unwrap_or_else(Instant::now);
            self.moved_at = self.moved_at.max(at);
        }
        self.moved_at
    }

    fn poll_sink(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let mut sink = Pin::new(&mut *self.sink);
        match self.kind {
            Kind::Send => {
                if self.item.is_some() {
                    ready!(sink.as_mut().poll_ready(cx))?;
                    let frame = self.item.take().expect("checked just above");
                    sink.as_mut().start_send(frame)?;
                }
                sink.poll_flush(cx)
            }
            Kind::Flush => sink.poll_flush(cx),
            Kind::Close => sink.poll_close(cx),
        }
    }
}

impl<W> Future for Op<'_, W>
where
    W: Sink<FrameBody, Error = io::Error> + WriteProgress + Unpin,
{
    type Output = io::Result<()>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        let out = this.poll_sink(cx);
        // Every poll is a look: bytes that went out during this one
        // restart the window at the moment they did.
        this.observe();
        out
    }
}
