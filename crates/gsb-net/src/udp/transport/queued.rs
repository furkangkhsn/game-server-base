//! An established session's endpoint on its way from the demux to the
//! accept loop (BACKLOG B74). The session's accept has already gone to
//! the client, so an endpoint that never reaches the accept loop is a
//! lost connection: dropped with the listener's queue when it closes or
//! goes away, or taken by a pending accept whose door closed meanwhile.
//! The item counts itself as it drops — the one place that sees every
//! such end, whichever side drops it and whenever — unless it was
//! adopted (or refused at the send, which the demux counts itself).

use gsb_core::metrics::TransportCounters;

use crate::metrics::Flusher;
use crate::transport::Endpoint;

/// A queued endpoint, counted as `udp_sessions_unaccepted_closed` if it
/// is dropped still holding its endpoint.
pub(in crate::udp) struct Queued {
    endpoint: Option<Endpoint>,
    metrics: crate::TransportMetrics,
}

impl Queued {
    pub(in crate::udp) fn new(endpoint: Endpoint, metrics: crate::TransportMetrics) -> Self {
        Self {
            endpoint: Some(endpoint),
            metrics,
        }
    }

    /// The endpoint, no longer counted: the accept loop takes it, or the
    /// demux takes it back from a refused send (counted there).
    pub(in crate::udp) fn into_endpoint(mut self) -> Endpoint {
        self.endpoint
            .take()
            .expect("a queued endpoint is taken once")
    }
}

impl Drop for Queued {
    fn drop(&mut self) {
        if self.endpoint.is_some() {
            // One sample of its own (a rare end — a closing listener's
            // queue, at most its capacity), past a full channel.
            let lost = TransportCounters {
                udp_sessions_unaccepted_closed: 1,
                ..Default::default()
            };
            Flusher::new(self.metrics.take()).flush(lost, true);
        }
    }
}
