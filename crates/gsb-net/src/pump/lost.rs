//! What the stream pumps lose, counted (BACKLOG B66) and handed to the
//! collector through the transport scope (`crate::metrics`). A child of
//! [`super`].
//!
//! Two losses, both at a pump's end and both at most once per pump, so a
//! one-shot sample (a fresh [`Flusher`], sent as a last one: past a full
//! channel) is all either needs — no periodic state in the pumps:
//!
//! - the reader's frame the connection actor never took: the reader had
//!   read it and was handing it over when the actor's inbox closed (the
//!   actor closes it as a server-decided end finishes — B60), classified
//!   by [`FrameKind`] so an RPC request lands in its own ledger term;
//! - the writer's frames it never wrote because it ended on a failed
//!   write or a write stall: the rest of the batch it was writing, and
//!   every batch still queued in the connection's outbound channel. The
//!   room and the connection actor counted those as shipped/sent — the
//!   channel took them; this is where they end.

use gsb_core::channel::{FrameBatch, Inbox};
use gsb_core::conn::FrameKind;
use gsb_core::metrics::TransportCounters;

use crate::TransportMetrics;
use crate::metrics::Flusher;

/// Count the frame (opcode `op`) a reader pump could not hand to a closed
/// inbox, and send it.
pub(super) fn reader_frame_dropped_closed(metrics: TransportMetrics, op: u16) {
    let mut totals = TransportCounters::default();
    let n = match FrameKind::of(op) {
        FrameKind::Request => &mut totals.stream_requests_dropped_closed,
        FrameKind::Action => &mut totals.stream_actions_dropped_closed,
        FrameKind::Control => &mut totals.stream_control_frames_dropped_closed,
    };
    *n = 1;
    Flusher::new(metrics).flush(totals, true);
}

/// The writer pump's unwritten frames (see the module docs).
#[derive(Debug, Default)]
pub(super) struct Unwritten {
    frames: u64,
    batches: u64,
    /// The stall verdict found the mailbox full with no slot reserved
    /// (see `crate::pump::verdict`).
    verdicts_deferred: u64,
}

impl Unwritten {
    /// The writer stopped inside a batch: `left` of its frames (the
    /// failed or stalled one included) were never written.
    pub(super) fn rest_of_batch(&mut self, left: usize) {
        self.frames += left as u64;
    }

    /// Close the outbound channel (every later send fails, and the sender
    /// counts that as its own) and count what it still holds. On an
    /// ordinary end (every sender gone) it holds nothing.
    pub(super) fn drain(&mut self, out_rx: &mut Inbox<FrameBatch>) {
        out_rx.close();
        while let Ok(batch) = out_rx.try_recv() {
            self.batches += 1;
            self.frames += batch.len() as u64;
        }
    }

    /// The stall verdict could only be posted after the close (see
    /// `crate::pump::verdict`).
    pub(super) fn verdict_deferred(&mut self) {
        self.verdicts_deferred += 1;
    }

    /// Send the count (nothing when nothing was lost).
    pub(super) fn report(self, metrics: TransportMetrics) {
        Flusher::new(metrics).flush(
            TransportCounters {
                stream_frames_unwritten: self.frames,
                stream_batches_unwritten: self.batches,
                writer_verdicts_deferred: self.verdicts_deferred,
                ..Default::default()
            },
            true,
        );
    }
}
