//! The periodic metrics flush: this actor's counters, as one delta
//! sample, on the bounded metrics channel.

use std::time::Instant;

use tokio::sync::mpsc::error::TrySendError;

use crate::conn::*;
use crate::metrics::{ConnSample, MetricsEvent};

impl super::ConnectionActor {
    /// Flush this connection's counters as a delta sample when there is
    /// new data and the flush interval has passed (or unconditionally for
    /// the final flush). Synchronous: the only check is an `Instant`
    /// comparison on each inbound frame, so the actor's only await stays
    /// the inbox `recv`.
    ///
    /// The flushed baseline advances only when the channel TOOK the
    /// sample (B59). A full channel drops the sample, not its deltas:
    /// they stay unflushed and the next flush (the final one at the
    /// latest) carries them together with the newer ones, and the drop
    /// itself is counted in `metrics_dropped`. Before B59 the baseline
    /// advanced first, so a dropped sample took its deltas with it.
    pub(super) fn maybe_flush_metrics(&mut self, last: bool) {
        // The server-close verdict rides the FINAL sample only (one per
        // session), and forces it out even when every delta is zero — a
        // connection refused at birth has sent and received nothing, and
        // its refusal must still be counted.
        let server_close = if last { self.server_close } else { None };
        let sample = ConnSample {
            conn: self.conn,
            bytes_in: self.m_in_bytes - self.m_flushed_in_bytes,
            bytes_out: self.m_out_bytes - self.m_flushed_out_bytes,
            frames_in: self.m_in_frames - self.m_flushed_in_frames,
            frames_out: self.m_out_frames - self.m_flushed_out_frames,
            actions_dropped: self.m_actions_dropped,
            actions_dropped_closed: self.m_actions_dropped_closed,
            requests_dropped_closed: self.m_requests_dropped_closed,
            requests_dropped_full: self.m_requests_dropped_full,
            requests_no_room: self.m_requests_no_room,
            requests_unprocessed: self.m_requests_unprocessed,
            actions_unprocessed: self.m_actions_unprocessed,
            control_frames_unprocessed: self.m_control_frames_unprocessed,
            heartbeats_throttled_preauth: self.m_preauth_hb_extra - self.m_flushed_preauth_hb_extra,
            heartbeats_throttled_authed: self.m_hb_extra - self.m_flushed_hb_extra,
            frames_out_closed: self.m_frames_out_closed,
            close_notices_dropped: self.m_close_notices_dropped,
            metrics_dropped: self.m_metrics_dropped,
            violations: self.m_violations,
            input_rate_limited: self.m_input_limited,
            server_close,
            last,
        };
        if is_empty(&sample) {
            return;
        }
        if !last && Instant::now().duration_since(self.m_last_flush) < METRICS_FLUSH_EVERY {
            return;
        }
        self.m_last_flush = Instant::now();
        // A3: bounded channel + synchronous `try_send`. Full: the sample
        // is dropped and counted in the next one; the deltas stay (see
        // above). Closed: the collector is gone (the process is coming
        // down) and nothing will read another sample.
        match self.metrics.try_send(MetricsEvent::Conn(sample)) {
            Err(TrySendError::Full(_)) => self.m_metrics_dropped += 1,
            Ok(()) | Err(TrySendError::Closed(_)) => self.mark_flushed(),
        }
    }

    /// The sample went out: every delta is now flushed.
    fn mark_flushed(&mut self) {
        self.m_flushed_in_bytes = self.m_in_bytes;
        self.m_flushed_in_frames = self.m_in_frames;
        self.m_flushed_out_bytes = self.m_out_bytes;
        self.m_flushed_out_frames = self.m_out_frames;
        self.m_actions_dropped = 0;
        self.m_actions_dropped_closed = 0;
        self.m_requests_dropped_closed = 0;
        self.m_requests_dropped_full = 0;
        self.m_requests_no_room = 0;
        self.m_requests_unprocessed = 0;
        self.m_actions_unprocessed = 0;
        self.m_control_frames_unprocessed = 0;
        self.m_flushed_preauth_hb_extra = self.m_preauth_hb_extra;
        self.m_flushed_hb_extra = self.m_hb_extra;
        self.m_frames_out_closed = 0;
        self.m_close_notices_dropped = 0;
        self.m_metrics_dropped = 0;
        self.m_violations = 0;
        self.m_input_limited = 0;
    }
}

/// Nothing to say: every delta is zero and no verdict rides along.
fn is_empty(s: &ConnSample) -> bool {
    s.bytes_in == 0
        && s.frames_in == 0
        && s.bytes_out == 0
        && s.frames_out == 0
        && s.actions_dropped == 0
        && s.actions_dropped_closed == 0
        && s.requests_dropped_closed == 0
        && s.requests_dropped_full == 0
        && s.requests_no_room == 0
        && s.requests_unprocessed == 0
        && s.actions_unprocessed == 0
        && s.control_frames_unprocessed == 0
        && s.heartbeats_throttled_preauth == 0
        && s.heartbeats_throttled_authed == 0
        && s.frames_out_closed == 0
        && s.close_notices_dropped == 0
        && s.metrics_dropped == 0
        && s.violations == 0
        && s.input_rate_limited == 0
        && s.server_close.is_none()
}
