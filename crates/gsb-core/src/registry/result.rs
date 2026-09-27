//! Handing a room's match result to the result sink (the control plane's
//! result seam — see [`crate::room::GameLogic::match_result`]), and
//! counting the result the sink refused (BACKLOG B57).
//!
//! The room (or each shard of a sharded room) reports its result once,
//! as it stops, with a synchronous `try_send` — a slow consumer must not
//! stall the teardown. A refused result used to leave only a log line.
//! It is now also told to the metrics collector as a
//! [`MetricsEvent::MatchResultDropped`], by cause: a FULL sink (the
//! consumer is not reading fast enough — or at all) or a CLOSED one (the
//! consumer dropped its receiver). The room's own counters cannot carry
//! it: a stopping room sends no further sample. The event is itself a
//! best-effort `try_send`; when the collector is gone too (the process is
//! stopping) nothing is left to report it to.

use tokio::sync::mpsc;
use tokio::sync::mpsc::error::TrySendError;

use crate::channel::Mailbox;
use crate::metrics::{MatchResultDrop, MetricsEvent};
use crate::registry::MatchResult;

/// Hand `result` to `sink`; on refusal, tell the collector why and
/// return the cause (for the caller's log line).
pub(crate) fn send_match_result(
    sink: &Mailbox<MatchResult>,
    result: MatchResult,
    metrics: &mpsc::Sender<MetricsEvent>,
) -> Result<(), MatchResultDrop> {
    let cause = match sink.try_send(result) {
        Ok(()) => return Ok(()),
        Err(TrySendError::Full(_)) => MatchResultDrop::Full,
        Err(TrySendError::Closed(_)) => MatchResultDrop::Closed,
    };
    let _ = metrics.try_send(MetricsEvent::MatchResultDropped(cause));
    Err(cause)
}
