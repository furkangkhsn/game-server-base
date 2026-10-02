//! The reader pump's idle window, and the stall rule it shares with the
//! rUDP demux's idle sweep (BACKLOG F72). A child of [`super`].
//!
//! An idle window is a deadline on the clock, and the clock does not
//! stop when the PROCESS does: after a stall longer than the window (a
//! swap storm, a VM pause, a starved runtime) every live session's
//! deadline is already due on wake, and closing them all `idle_timeout`
//! books the server's own silence as the clients'. A deadline cannot
//! tell why nothing arrived, but it can tell that it fired LATE — and a
//! fire later than [`IDLE_STALL_GRACE`] means the process was not
//! running when the silence would have been observed. So:
//!
//! - a deadline that fires more than the grace late RESTARTS the window
//!   (counted: `idle_windows_restarted_late`);
//! - once per silence: a client still silent when the restarted window
//!   fires is closed then, however late that fire is — a process stalled
//!   for good cannot keep a half-open session alive, the bound is two
//!   windows plus the stalls; a frame re-arms the restart;
//! - a deadline on time closes as it always has — after one look at the
//!   socket (below).
//!
//! **The look before the verdict** (the F34 case): the timer and the
//! socket's readiness are separate driver events, and a frame already in
//! the socket can still be unreported when the timer wakes the pump. Before
//! the verdict the pump yields once — the runtime polls its IO driver
//! before it runs a yielded task again — and reads the stream once more
//! without waiting. A frame that was there wins; nothing there, the
//! client really was silent for the whole window.

use std::time::Duration;

use futures::{FutureExt, Stream, StreamExt};
use gsb_core::id::ConnectionId;
use gsb_core::metrics::TransportCounters;
use tokio::time::Instant;
use tracing::info;

use crate::TransportMetrics;
use crate::metrics::Flusher;

/// How late an idle deadline may fire and still be the client's silence;
/// later is the process's stall (see the module docs). Shared by every
/// idle window the transports run (the stream doors' reader pumps, the
/// rUDP demux's sweep).
///
/// Why 250 ms: far above what a RUNNING process's timers see — tokio's
/// wheel is 1 ms, and 500 timers measured on this project's machine fired
/// at most 18 ms late quiet and 12 ms late with the process pinned to two
/// cores beside 24 busy loops — and far below what a stalled one shows:
/// the same process at nice 19 against that load, starved rather than
/// slowed, saw p50 17 ms and p99 1.3 s. It is also the engine's existing
/// "a healthy task never takes this long" bound (`CUT_GRACE`, where F29
/// measured ~200 ms slips under heavy load). A fixed bound, not a fraction
/// of the window: it judges the PROCESS, which is as stalled at a 250 ms
/// lateness whatever window the operator chose.
pub const IDLE_STALL_GRACE: Duration = Duration::from_millis(250);

/// Whether a deadline that fired `late` past itself is the process's
/// stall rather than the client's silence.
pub(crate) fn stalled(late: Duration) -> bool {
    late > IDLE_STALL_GRACE
}

/// Count `n` restarted windows: one sample, sent past a full channel
/// (a restart is rare — the shape of the pumps' end-of-life counts).
pub(crate) fn count_restarts(metrics: TransportMetrics, n: u64) {
    let totals = TransportCounters {
        idle_windows_restarted_late: n,
        ..Default::default()
    };
    Flusher::new(metrics).flush(totals, true);
}

/// What the window gave the reader.
pub(super) enum Next<T> {
    /// The stream's next item (`None`: the stream ended).
    Item(Option<T>),
    /// The window ran out: the client was silent for all of it.
    Idle,
}

/// One reader pump's idle window.
pub(super) struct IdleWindow {
    conn: ConnectionId,
    window: Duration,
    /// The current silence already had its restart.
    restarted: bool,
    metrics: TransportMetrics,
}

impl IdleWindow {
    pub(super) fn new(conn: ConnectionId, window: Duration, metrics: TransportMetrics) -> Self {
        Self {
            conn,
            window,
            restarted: false,
            metrics,
        }
    }

    /// The window's length.
    pub(super) fn window(&self) -> Duration {
        self.window
    }

    /// The next item of `stream`, or [`Next::Idle`] when the window runs
    /// out. One awaited source: the deadline wraps the read (a ready
    /// frame always wins), it does not multiplex a second one.
    pub(super) async fn next<S>(&mut self, stream: &mut S) -> Next<S::Item>
    where
        S: Stream + Unpin,
    {
        loop {
            let deadline = Instant::now() + self.window;
            if let Ok(item) = tokio::time::timeout_at(deadline, stream.next()).await {
                self.restarted = false;
                return Next::Item(item);
            }
            let late = Instant::now().saturating_duration_since(deadline);
            if stalled(late) && !self.restarted {
                self.restarted = true;
                count_restarts(self.metrics.clone(), 1);
                info!(
                    conn = %self.conn,
                    ?late,
                    "reader pump: the idle deadline fired late (a stalled process); \
                     the window restarts"
                );
                continue;
            }
            // The look before the verdict (module docs).
            tokio::task::yield_now().await;
            if let Some(item) = stream.next().now_or_never() {
                self.restarted = false;
                return Next::Item(item);
            }
            return Next::Idle;
        }
    }
}
