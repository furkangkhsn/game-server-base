//! The socket's own byte count, for the write-stall clock (BACKLOG B15),
//! where the bytes reach the socket somewhere the framing writer cannot
//! see them.
//!
//! Two doors have such a place:
//!
//! - **TLS**: rustls holds up to 64 KiB of ciphertext of its own; the
//!   framing writer only sees plaintext handed to rustls, and a frame's
//!   tail drains inside `poll_flush`, which reports no count. [`Wire`]
//!   sits BENEATH rustls, around the TCP stream, and counts what the
//!   socket takes — ciphertext included, whoever writes it.
//! - **WebSocket**: one socket-writer task writes the socket, a queue
//!   below the pump; the pump sees its count only when it looks. The
//!   task records WHEN each byte went out, so the window restarts at the
//!   byte, not at the look — without it the verdict came up to two
//!   windows after the socket last took anything.
//!
//! Lock-free: two atomics, written by whoever writes the socket, read by
//! the writer pump (see [`crate::pump::WriteProgress`]).

use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::task::{Context, Poll};
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::time::Instant;

/// Bytes the socket has taken, and when it last took one.
#[derive(Debug, Clone)]
pub(crate) struct WireCount(Arc<Inner>);

#[derive(Debug)]
struct Inner {
    /// What `at_ns` counts from.
    base: Instant,
    bytes: AtomicU64,
    /// Nanoseconds from `base` to the last write that took a byte.
    at_ns: AtomicU64,
}

impl WireCount {
    pub(crate) fn new() -> Self {
        Self(Arc::new(Inner {
            base: Instant::now(),
            bytes: AtomicU64::new(0),
            at_ns: AtomicU64::new(0),
        }))
    }

    /// The socket took `n` bytes just now. The time is stored before the
    /// count, so whoever sees the new count sees this time (or a later
    /// one — the TLS read path may write too, hence `fetch_max`).
    pub(crate) fn wrote(&self, n: usize) {
        if n == 0 {
            return;
        }
        let at = u64::try_from(self.0.base.elapsed().as_nanos()).unwrap_or(u64::MAX);
        self.0.at_ns.fetch_max(at, Ordering::Release);
        self.0.bytes.fetch_add(n as u64, Ordering::Release);
    }

    /// Bytes taken so far (monotonic).
    pub(crate) fn bytes(&self) -> u64 {
        self.0.bytes.load(Ordering::Acquire)
    }

    /// When the socket last took a byte (the count's birth, if never).
    pub(crate) fn last_at(&self) -> Instant {
        self.0.base + Duration::from_nanos(self.0.at_ns.load(Ordering::Acquire))
    }
}

/// A stream that counts every byte its writes hand the socket into a
/// [`WireCount`]; reads pass straight through. The TLS door wraps the
/// TCP stream in it before the handshake, so rustls writes through it.
pub(crate) struct Wire<S> {
    inner: S,
    count: WireCount,
}

impl<S> Wire<S> {
    pub(crate) fn new(inner: S, count: WireCount) -> Self {
        Self { inner, count }
    }
}

impl<S: AsyncRead + Unpin> AsyncRead for Wire<S> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_read(cx, buf)
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for Wire<S> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        let out = Pin::new(&mut this.inner).poll_write(cx, buf);
        if let Poll::Ready(Ok(n)) = out {
            this.count.wrote(n);
        }
        out
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        let out = Pin::new(&mut this.inner).poll_write_vectored(cx, bufs);
        if let Poll::Ready(Ok(n)) = out {
            this.count.wrote(n);
        }
        out
    }

    fn is_write_vectored(&self) -> bool {
        self.inner.is_write_vectored()
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
    }
}

#[cfg(test)]
mod tests;
