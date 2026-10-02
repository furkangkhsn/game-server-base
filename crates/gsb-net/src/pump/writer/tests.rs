//! The stall clock's bound, on a paused clock (BACKLOG B15c).
//!
//! A door whose socket is written by ANOTHER task (WebSocket: the
//! socket-writer task behind a bounded queue) moves its byte count
//! without waking the pump: the pump is parked on a queue slot, and a
//! slot frees only when a whole frame is out. The pump first SEES such
//! bytes at its deadline. If the window then restarts at the moment of
//! the look rather than at the byte, the verdict comes up to two windows
//! after the socket last took anything — a dead peer kept twice as long
//! as `write_stall_secs` says.

use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use futures::Sink;
use tokio::time::Instant;

use gsb_core::channel::{FrameBatch, channel};
use gsb_core::conn::{ConnIn, ServerClose};
use gsb_core::id::ConnectionId;
use gsb_protocol::FrameBody;

use crate::pump::{PumpTimeouts, WriteProgress, spawn_pumps};
use crate::wire::WireCount;

const WINDOW: Duration = Duration::from_secs(10);

/// A sink whose queue is full and stays full (it never wakes the pump),
/// while the byte count is moved by someone else — the WebSocket door's
/// shape, one queue above its socket-writer task, with the same count.
struct Remote {
    count: WireCount,
}

impl Sink<FrameBody> for Remote {
    type Error = std::io::Error;

    fn poll_ready(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Poll::Pending
    }

    fn start_send(self: Pin<&mut Self>, _item: FrameBody) -> std::io::Result<()> {
        Ok(())
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_close(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Poll::Pending
    }
}

impl WriteProgress for Remote {
    fn bytes_written(&self) -> u64 {
        self.count.bytes()
    }

    fn last_write_at(&self) -> Option<Instant> {
        Some(self.count.last_at())
    }
}

/// Run the pump over [`Remote`] with one frame to write; another task
/// moves the count at each offset in `bytes_at`. Returns
/// when the stall verdict arrived, measured from the pump's start.
async fn verdict_after(bytes_at: &[Duration]) -> Option<Duration> {
    let count = WireCount::new();
    let (in_tx, mut in_rx) = channel::<ConnIn>(4);
    let (out_tx, out_rx) = channel::<FrameBatch>(1);
    out_tx
        .try_send(vec![FrameBody::new(7, vec![0; 8])])
        .expect("room");
    let start = Instant::now();
    let (read, write) = spawn_pumps(
        ConnectionId(71),
        futures::stream::pending::<std::io::Result<FrameBody>>(),
        Remote {
            count: count.clone(),
        },
        in_tx,
        out_rx,
        PumpTimeouts {
            idle: None,
            write_stall: Some(WINDOW),
        },
        None,
    );
    let mover = {
        let at = bytes_at.to_vec();
        tokio::spawn(async move {
            for t in at {
                tokio::time::sleep_until(start + t).await;
                count.wrote(100);
            }
        })
    };
    let got = tokio::time::timeout(WINDOW * 10, in_rx.recv()).await;
    let took = start.elapsed();
    mover.abort();
    read.abort();
    write.abort();
    match got {
        Ok(Some(ConnIn::ServerClosed { cause, .. })) => {
            assert_eq!(cause, ServerClose::WriteStall);
            Some(took)
        }
        Ok(other) => panic!("expected the stall verdict, got {other:?}"),
        Err(_) => None,
    }
}

/// THE BOUND: the verdict comes one window after the LAST byte, however
/// late the pump got to see it — not one window after the look.
#[tokio::test(start_paused = true)]
async fn the_verdict_comes_one_window_after_the_last_byte_seen_late() {
    let last = Duration::from_millis(100);
    let took = verdict_after(&[last]).await.expect("a verdict");
    assert!(
        took >= WINDOW + last && took <= WINDOW + last + Duration::from_millis(5),
        "the socket last took a byte at {last:?}; the verdict came at \
         {took:?}, not one {WINDOW:?} window after it"
    );
}

/// The converse: bytes moving behind the pump's back, each less than a
/// window after the last, keep the session however long it lasts.
#[tokio::test(start_paused = true)]
async fn bytes_seen_only_at_deadlines_keep_a_draining_peer() {
    let every = WINDOW - Duration::from_millis(1);
    let moves: Vec<Duration> = (1..=8).map(|i| every * i).collect();
    let took = verdict_after(&moves)
        .await
        .expect("a verdict once they stop");
    let last = *moves.last().expect("eight moves");
    assert!(
        took >= last + WINDOW && took <= last + WINDOW + Duration::from_millis(5),
        "bytes moved until {last:?}; the verdict came at {took:?}"
    );
}
