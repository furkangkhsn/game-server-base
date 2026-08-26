//! The global tick service.
//!
//! A single ticker task emits [`TickInfo`] on a `tokio::sync::broadcast`
//! channel at a fixed rate. Every room actor subscribes to that channel;
//! the broadcast receive is the room's *only* awaited source.
//!
//! Properties:
//! - `send` is non-blocking: even if every receiver is lagging or gone, the
//!   ticker can never stall (lagging receivers surface it as `Lagged`, which
//!   the room turns into one catch-up step).
//! - Aborting the ticker task closes the channel: all subscribers observe
//!   `Closed`, which doubles as the global stop signal for rooms.
//! - No cross-room coordination: each room advances its own drift from the
//!   wall-clock timestamps carried in [`TickInfo`].

use std::time::{Duration, Instant};

use tokio::sync::broadcast;
use tokio::task::JoinHandle;

use crate::error::CoreError;

/// One tick of the global clock.
#[derive(Debug, Clone, Copy)]
pub struct TickInfo {
    /// Global tick index (monotonic, starts at 1).
    pub tick: u64,
    /// Wall-clock instant at which the tick was emitted.
    pub at: Instant,
}

/// Handle to the global tick service: the broadcast sender plus the global
/// rate. Cheap to clone (the sender is reference-counted internally).
#[derive(Debug, Clone)]
pub struct Ticker {
    tx: broadcast::Sender<TickInfo>,
    hz: f64,
}

impl Ticker {
    /// Create the broadcast channel and spawn the ticker task.
    ///
    /// Returns the handle and the task handle. Aborting the task closes the
    /// channel for all subscribers.
    ///
    /// Returns [`CoreError::InvalidTickRate`] instead of panicking when
    /// `hz` has no usable period (`<= 0`, NaN/±inf, or so high that the
    /// derived period rounds below one nanosecond). The registry validates
    /// room rates before a room exists, but `spawn` itself is public API —
    /// tests and platform embedders call it directly, and the global rate
    /// crosses exactly this boundary from config — so the invariant is
    /// enforced here as a typed error rather than trusted or panicked on.
    pub fn spawn(hz: f64, buffer: usize) -> Result<(Self, JoinHandle<()>), CoreError> {
        // Validate BEFORE deriving anything: `from_secs_f64(1.0 / hz)`
        // panics for hz <= 0 (a negative period) and NaN, overflows for a
        // denormal-tiny rate whose reciprocal exceeds the Duration range,
        // and silently truncates to zero for an absurdly high rate (which
        // would busy-loop the runtime instead of ticking). The float
        // division itself never panics, so this guard is total.
        let period = if hz.is_finite() && hz > 0.0 {
            Duration::try_from_secs_f64(1.0 / hz)
                .ok()
                .filter(|period| !period.is_zero())
        } else {
            None
        };
        let Some(period) = period else {
            return Err(CoreError::InvalidTickRate { rate: hz });
        };
        let (tx, _first_rx) = broadcast::channel(buffer);
        let ticker = Self { tx: tx.clone(), hz };
        let handle = tokio::spawn(async move {
            let mut next = Instant::now() + period;
            let mut tick = 0u64;
            loop {
                let delay = next.saturating_duration_since(Instant::now());
                tokio::time::sleep(delay).await;
                tick += 1;
                next += period;
                // Fell behind (runtime starvation): resync to the wall
                // clock instead of bursting — room actors derive dt from
                // the timestamps, so the gap is already visible to them.
                if next < Instant::now() {
                    next = Instant::now() + period;
                }
                // Non-blocking by construction of the broadcast channel:
                // errors only when no subscriber exists at all. Keep
                // running; rooms may subscribe later.
                let _ = tx.send(TickInfo {
                    tick,
                    at: Instant::now(),
                });
            }
        });
        Ok((ticker, handle))
    }

    /// The global rate in ticks per second. A room's `tick_hz` must divide
    /// this (the room steps on every k-th global tick).
    pub fn hz(&self) -> f64 {
        self.hz
    }

    /// The global tick period.
    ///
    /// Cannot panic: a `Ticker` value only exists for a rate that already
    /// produced a valid, non-zero period in [`Ticker::spawn`].
    pub fn period(&self) -> Duration {
        Duration::from_secs_f64(1.0 / self.hz)
    }

    /// Subscribe a room (or test) to the global tick stream.
    pub fn subscribe(&self) -> broadcast::Receiver<TickInfo> {
        self.tx.subscribe()
    }
}

#[cfg(test)]
mod tests;
