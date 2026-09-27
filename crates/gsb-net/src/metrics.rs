//! The transport's side of the metrics path (BACKLOG B58).
//!
//! The network layer reaches the collector the way every producer does,
//! through the bounded metrics channel of `gsb_core::metrics` — the same
//! direction as every other `gsb-core` type this crate already uses (the
//! connection's mailboxes, `ConnIn`), so no layer is inverted and the
//! collector learns nothing about transports: it sums
//! [`TransportCounters`] deltas like it sums a connection's.
//!
//! A transport task (the rUDP demux, an rUDP writer, a WebSocket reader,
//! a handshake door's intake) keeps its counters in its own state and
//! owns one [`Flusher`]: at most once per [`FLUSH_EVERY`] while it works,
//! and once more as it ends, the flusher sends the growth since the last
//! sample the channel TOOK (the B59 rule: a sample dropped on a full
//! channel is counted and its deltas ride the next one). The last one
//! goes out past a full channel, from a spawned sender, when a runtime is
//! there to spawn on. A transport built without a metrics sender (tests,
//! embedders) counts as before and sends nothing.

use std::time::{Duration, Instant};

use gsb_core::metrics::{MetricsEvent, TransportCounters};
use tokio::sync::mpsc;
use tokio::sync::mpsc::error::TrySendError;

/// Where a transport's samples go: the collector's metrics channel. Set
/// by the composition root on each transport's configuration; `None`
/// sends nothing.
pub type TransportMetrics = Option<mpsc::Sender<MetricsEvent>>;

/// How often a busy transport task flushes (the connection actor's
/// cadence).
pub(crate) const FLUSH_EVERY: Duration = Duration::from_millis(500);

/// One transport task's flush state (see the module docs).
#[derive(Debug)]
pub(crate) struct Flusher {
    tx: TransportMetrics,
    /// The totals the channel last took.
    flushed: TransportCounters,
    /// Samples this task dropped on a full channel (cumulative).
    dropped: u64,
    last: Instant,
}

impl Flusher {
    pub(crate) fn new(tx: TransportMetrics) -> Self {
        Self {
            tx,
            flushed: TransportCounters::default(),
            dropped: 0,
            last: Instant::now(),
        }
    }

    /// Whether a periodic flush is due (cheap: checked before the caller
    /// builds its totals).
    pub(crate) fn due(&self) -> bool {
        self.tx.is_some() && self.last.elapsed() >= FLUSH_EVERY
    }

    /// Send the growth of `totals` since the last sample the channel took
    /// (nothing when there is none). `last`: the task is ending — the
    /// sample goes out past a full channel.
    pub(crate) fn flush(&mut self, mut totals: TransportCounters, last: bool) {
        let Some(tx) = &self.tx else {
            return;
        };
        totals.metrics_dropped = self.dropped;
        let delta = totals.since(&self.flushed);
        if delta.is_zero() {
            return;
        }
        self.last = Instant::now();
        match tx.try_send(delta.event()) {
            Ok(()) => self.flushed = totals,
            Err(TrySendError::Full(ev)) if last => {
                // The task is ending: no next sample to carry this one.
                // Hand it to a spawned sender (never awaited here) when a
                // runtime is there to spawn on.
                if let Ok(rt) = tokio::runtime::Handle::try_current() {
                    let tx = tx.clone();
                    rt.spawn(async move {
                        let _ = tx.send(ev).await;
                    });
                }
                self.flushed = totals;
            }
            Err(TrySendError::Full(_)) => self.dropped += 1,
            // The collector is gone: nothing will read another sample.
            Err(TrySendError::Closed(_)) => self.flushed = totals,
        }
    }
}

#[cfg(test)]
mod tests;
