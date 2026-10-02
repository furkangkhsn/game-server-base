//! The reader pump's idle window against a stalled process (BACKLOG
//! F72), on the paused clock: a stall is the clock jumping past the
//! window at once (`tokio::time::advance`), the way a frozen process
//! finds every deadline already due when it wakes.
//!
//! - a deadline that fires more than [`IDLE_STALL_GRACE`] late restarts
//!   the window and counts it (`idle_windows_restarted_late`);
//! - once per silence: a second late fire closes, a frame re-arms it;
//! - a deadline on time (or late by no more than the grace) closes as
//!   before;
//! - a frame readable when the deadline fires wins over the verdict.

use super::*;

use std::sync::atomic::{AtomicBool, Ordering};

use futures::Stream;
use gsb_core::channel::Inbox;
use tokio::time::Instant;

use crate::pump::IDLE_STALL_GRACE;

const WINDOW: Duration = Duration::from_secs(2);

/// The test's peer: frames arrive exactly when the test sends them.
struct Peer(mpsc::Receiver<std::io::Result<FrameBody>>);

impl Stream for Peer {
    type Item = std::io::Result<FrameBody>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.get_mut().0.poll_recv(cx)
    }
}

/// A socket holding one frame the IO driver has not reported yet. The
/// frame lands with the `at`-th read (a deadline's read — nothing found,
/// no wake asked: the readiness was never seen); its readiness is
/// reported by the runtime's next turn of other work (`driver`, a task
/// that read wakes), and from then on one read finds it.
struct UnseenFrame {
    at: u32,
    reads: u32,
    landed: Option<tokio::sync::oneshot::Sender<()>>,
    reported: Arc<AtomicBool>,
    taken: bool,
}

impl UnseenFrame {
    fn at(at: u32) -> Self {
        let (landed, driver) = tokio::sync::oneshot::channel::<()>();
        let reported = Arc::new(AtomicBool::new(false));
        let flag = reported.clone();
        tokio::spawn(async move {
            if driver.await.is_ok() {
                flag.store(true, Ordering::Release);
            }
        });
        Self {
            at,
            reads: 0,
            landed: Some(landed),
            reported,
            taken: false,
        }
    }
}

impl Stream for UnseenFrame {
    type Item = std::io::Result<FrameBody>;

    fn poll_next(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        this.reads += 1;
        if this.reads == this.at
            && let Some(landed) = this.landed.take()
        {
            let _ = landed.send(());
        }
        if !this.taken && this.reported.load(Ordering::Acquire) {
            this.taken = true;
            return Poll::Ready(Some(Ok(FrameBody::new(op::base::HEARTBEAT, Vec::new()))));
        }
        Poll::Pending
    }
}

/// The pump under test: idle window only; its inbox and its transport
/// samples (the outbound sender is returned so the writer stays up).
struct Rig {
    inbox: Inbox<ConnIn>,
    metrics: mpsc::Receiver<MetricsEvent>,
    _out: gsb_core::channel::Mailbox<FrameBatch>,
    t0: Instant,
}

async fn rig<R>(reader: R) -> Rig
where
    R: Stream<Item = std::io::Result<FrameBody>> + Unpin + Send + 'static,
{
    let (in_tx, inbox) = channel::<ConnIn>(8);
    let (out, out_rx) = channel::<FrameBatch>(1);
    let (metrics_tx, metrics) = mpsc::channel::<MetricsEvent>(64);
    let t0 = Instant::now();
    let timeouts = PumpTimeouts {
        idle: Some(WINDOW),
        write_stall: None,
    };
    let _ = spawn_pumps(
        ConnectionId(72),
        reader,
        Wedged,
        in_tx,
        out_rx,
        timeouts,
        Some(metrics_tx),
    );
    // Let the reader arm its window at `t0`.
    for _ in 0..4 {
        tokio::task::yield_now().await;
    }
    Rig {
        inbox,
        metrics,
        _out: out,
        t0,
    }
}

impl Rig {
    /// Stall the process for `d`: the clock jumps, every due timer fires
    /// at once.
    async fn stall(&self, d: Duration) {
        tokio::time::advance(d).await;
        for _ in 0..4 {
            tokio::task::yield_now().await;
        }
    }

    /// The next message, and when (since `t0`) it came.
    async fn next(&mut self) -> (ConnIn, Duration) {
        let msg = self.inbox.recv().await.expect("the reader reports");
        (msg, self.t0.elapsed())
    }

    /// The idle close, and when it came.
    async fn idle_close(&mut self) -> Duration {
        let (msg, at) = self.next().await;
        match msg {
            ConnIn::ServerClosed { cause, .. } => assert_eq!(cause, ServerClose::IdleTimeout),
            other => panic!("expected the idle close, got {other:?}"),
        }
        at
    }

    /// Restarted windows counted so far.
    fn restarts(&mut self) -> u64 {
        let mut n = 0;
        while let Ok(ev) = self.metrics.try_recv() {
            if let MetricsEvent::Transport(t) = ev {
                n += t.idle_windows_restarted_late;
            }
        }
        n
    }
}

/// THE PROPERTY: a deadline that fires a whole second late (a 3 s stall
/// against a 2 s window) restarts the window — the client is closed one
/// window after the wake, not at it — and the restart is counted.
#[tokio::test(start_paused = true)]
async fn a_stall_past_the_window_restarts_it_and_counts() {
    let mut r = rig(futures::stream::pending()).await;
    r.stall(Duration::from_secs(3)).await;
    assert!(r.inbox.is_empty(), "the stall is not the client's silence");
    assert_eq!(
        r.idle_close().await,
        Duration::from_secs(5),
        "one window after the wake"
    );
    assert_eq!(r.restarts(), 1);
}

/// Once per silence: a client still silent when the restarted window
/// fires — late again — is closed then.
#[tokio::test(start_paused = true)]
async fn a_second_late_fire_closes() {
    let mut r = rig(futures::stream::pending()).await;
    r.stall(Duration::from_secs(3)).await;
    r.stall(Duration::from_secs(3)).await;
    assert_eq!(r.idle_close().await, Duration::from_secs(6));
    assert_eq!(r.restarts(), 1);
}

/// A frame is a sign of life: the next stall restarts the window again.
#[tokio::test(start_paused = true)]
async fn a_frame_rearms_the_restart() {
    let (peer, rx) = mpsc::channel(4);
    let mut r = rig(Peer(rx)).await;
    r.stall(Duration::from_secs(3)).await;
    let hb = FrameBody::new(op::base::HEARTBEAT, Vec::new());
    peer.send(Ok(hb)).await.expect("the pump reads");
    let (msg, at) = r.next().await;
    assert!(matches!(msg, ConnIn::Frame(_)), "got {msg:?}");
    assert_eq!(at, Duration::from_secs(3));
    r.stall(Duration::from_secs(3)).await;
    assert_eq!(r.idle_close().await, Duration::from_secs(8));
    assert_eq!(r.restarts(), 2);
}

/// A deadline on time closes as it always has, and counts nothing.
#[tokio::test(start_paused = true)]
async fn an_on_time_deadline_closes() {
    let mut r = rig(futures::stream::pending()).await;
    assert_eq!(r.idle_close().await, WINDOW);
    assert_eq!(r.restarts(), 0);
}

/// The grace is the boundary: late by exactly the grace is still the
/// client's silence; a millisecond more is the process's stall.
#[tokio::test(start_paused = true)]
async fn the_grace_is_the_boundary() {
    let mut r = rig(futures::stream::pending()).await;
    r.stall(WINDOW + IDLE_STALL_GRACE).await;
    assert_eq!(r.idle_close().await, WINDOW + IDLE_STALL_GRACE);
    assert_eq!(r.restarts(), 0);

    let mut r = rig(futures::stream::pending()).await;
    let late = WINDOW + IDLE_STALL_GRACE + Duration::from_millis(1);
    r.stall(late).await;
    assert!(r.inbox.is_empty(), "a millisecond past the grace restarts");
    assert_eq!(r.idle_close().await, late + WINDOW);
    assert_eq!(r.restarts(), 1);
}

/// F34: a frame already in the socket when the deadline fires — its
/// readiness not yet seen — is read before the verdict, and the window
/// restarts from it.
#[tokio::test(start_paused = true)]
async fn a_frame_readable_at_the_deadline_wins() {
    // Reads: the window's first, then the deadline's (the frame lands).
    let mut r = rig(UnseenFrame::at(2)).await;
    let (msg, at) = r.next().await;
    assert!(matches!(msg, ConnIn::Frame(_)), "got {msg:?}");
    assert_eq!(at, WINDOW);
    assert_eq!(r.idle_close().await, WINDOW * 2);
    assert_eq!(r.restarts(), 0);
}

/// A frame the look finds is a sign of life like any other: it re-arms
/// the restart a stall had used up.
#[tokio::test(start_paused = true)]
async fn a_frame_found_by_the_look_rearms_the_restart() {
    // Reads: the first window's two, then the restarted window's two
    // (the frame lands with its on-time deadline's).
    let mut r = rig(UnseenFrame::at(4)).await;
    r.stall(Duration::from_secs(3)).await;
    let (msg, at) = r.next().await;
    assert!(matches!(msg, ConnIn::Frame(_)), "got {msg:?}");
    assert_eq!(at, Duration::from_secs(5));
    r.stall(Duration::from_secs(3)).await;
    assert!(r.inbox.is_empty(), "the next stall restarts again");
    assert_eq!(r.idle_close().await, Duration::from_secs(10));
    assert_eq!(r.restarts(), 2);
}
