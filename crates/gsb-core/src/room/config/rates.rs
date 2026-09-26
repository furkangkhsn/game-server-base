//! The rate rule a room must satisfy under the global ticker — the one
//! the registry applies at creation, public so a composition root can
//! refuse a configured room at startup by the SAME rule instead of a
//! copy that could drift from it.

use crate::error::CoreError;
use crate::room::RoomConfig;

impl RoomConfig {
    /// The room's step divisor under a global ticker at `global_hz`: the
    /// room steps on every k-th global tick. Refused, as the registry
    /// refuses the room:
    ///
    /// - [`CoreError::TickRate`] — `tick_hz` does not divide the global
    ///   rate (k must be a whole number `>= 1`; a zero, negative or
    ///   non-finite rate never does);
    /// - [`CoreError::KeepaliveRate`] — `keepalive_hz` (when enabled,
    ///   `> 0`) exceeds `tick_hz`: the room cannot keep alive faster
    ///   than it steps.
    pub fn step_divisor(&self, global_hz: f64) -> Result<u64, CoreError> {
        let run_every = (global_hz / self.tick_hz).round() as u64;
        if run_every < 1 || (global_hz - self.tick_hz * run_every as f64).abs() > 1e-3 {
            return Err(CoreError::TickRate {
                room: self.tick_hz,
                global: global_hz,
            });
        }
        // Keep-alive cannot run faster than the room's own tick: the
        // cadence would clamp to every step, the "silence when unchanged"
        // gain would be lost, and clients would receive fewer keep-alives
        // than configured.
        if self.keepalive_hz > 0.0 && self.keepalive_hz > self.tick_hz {
            return Err(CoreError::KeepaliveRate {
                keepalive: self.keepalive_hz,
                tick: self.tick_hz,
            });
        }
        Ok(run_every)
    }
}

#[cfg(test)]
mod tests;
