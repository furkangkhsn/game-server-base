//! Unit tests for [`super`] (moved out of the module file so the
//! implementation reads on its own; still a child module, so
//! `use super::*` reaches the parent's private items exactly as
//! before).

use super::*;
use crate::channel::channel;
use crate::channel::{FrameBatch, Mailbox};
use crate::error::CoreError;
use crate::id::{ConnectionId, EntityId, PlayerId, RoomId};
use crate::metrics::MetricsEvent;
use crate::room::config::FALLBACK_TICK_PERIOD;
use crate::ticker::TickInfo;
use gsb_protocol::FrameBody;
use std::collections::HashMap;
use std::time::Duration;
use std::time::Instant;
use tokio::sync::{broadcast, mpsc, oneshot};

mod binding;
mod counters;
mod fanout;
mod groups;
mod guardrails;
mod stubs;
mod tick;
use stubs::*;

/// Synchronous peek helper for the test above (the reply was already
/// sent by `handle_control`; a blocking recv would need a runtime).
fn tokio_sync_oneshot_peek(
    mut rx: oneshot::Receiver<Result<(EntityId, Mailbox<Action>), CoreError>>,
) -> Option<Result<(EntityId, Mailbox<Action>), CoreError>> {
    rx.try_recv().ok()
}

/// A metrics sender whose receiver is dropped immediately: the room's
/// per-step send fails and is ignored (the metric path is covered by
/// the dedicated metrics-flow test and by gsb-server's tests).
fn null_metrics_tx() -> mpsc::Sender<MetricsEvent> {
    let (tx, _rx) = mpsc::channel(1);
    tx
}

/// Wait until the logic reports `step_no` steps, then give the room a
/// moment to finish the in-flight step's fan-out (the step counter is
/// emitted in phase 3, fan-out is phase 4; the sleep is generous —
/// fan-out is microsecond-scale).
async fn wait_steps(steps: &mut mpsc::Receiver<u64>, n: u64) {
    while let Some(s) = tokio::time::timeout(Duration::from_secs(2), steps.recv())
        .await
        .expect("steps closed")
    {
        if s == n {
            tokio::time::sleep(Duration::from_millis(50)).await;
            return;
        }
    }
    panic!("steps channel closed before step {n}");
}

fn batch_frames(batch: &[FrameBody]) -> Vec<(u16, Vec<u8>)> {
    batch.iter().map(|f| (f.op, f.payload.to_vec())).collect()
}

/// Receive one batch with a timeout (the positive-side barrier: the
/// room flushed this connection, so its step's fan-out has reached it).
async fn next_batch_full(rx: &mut mpsc::Receiver<FrameBatch>) -> Vec<FrameBody> {
    tokio::time::timeout(Duration::from_secs(2), rx.recv())
        .await
        .expect("timed out waiting for a batch")
        .expect("out channel closed")
}

/// Take everything currently queued (after `wait_steps`, the fan-out of
/// every reported step has completed).
async fn drain_all(rx: &mut mpsc::Receiver<FrameBatch>) -> Vec<Vec<FrameBody>> {
    let mut out = Vec::new();
    while let Ok(batch) = rx.try_recv() {
        out.push(batch);
    }
    out
}
