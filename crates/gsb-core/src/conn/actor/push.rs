//! An event's CLIENT leg (`docs/EVENTS.md` §6.2): the registry routed it
//! here; the actor writes it as a `PUSH` frame on its own outbound and
//! never waits — a slow client loses the push, counted, the way a room's
//! fan-out loses a batch.

use gsb_protocol::{base, op};

use crate::channel::TrySend;
use crate::events::PublisherId;

impl super::ConnectionActor {
    /// Write one `PUSH` frame (`Push { publisher, seq, op, body, stream }`)
    /// with a synchronous `try_send`: queued → `pushes_written` (and the
    /// frame's bytes in `frames_out`/`bytes_out`), a full outbound →
    /// `pushes_dropped_outbound_full`, a gone writer →
    /// `pushes_dropped_outbound_gone`. The body was encoded once by the
    /// publisher; each recipient's frame carries its own copy (the
    /// frame's header — publisher, number, stream — is per recipient).
    pub(super) fn on_push(
        &mut self,
        publisher: PublisherId,
        stream: u64,
        seq: u64,
        event_op: u16,
        body: bytes::Bytes,
    ) {
        let msg = base::Push {
            publisher: publisher.0,
            seq,
            op: u32::from(event_op),
            body: body.to_vec(),
            stream,
        };
        let Some(fb) = self.table.frame(op::base::PUSH, &msg) else {
            return;
        };
        let bytes = 2 + fb.payload.len() as u64;
        match crate::channel::try_send(&self.out, vec![fb]) {
            TrySend::Sent => {
                self.m_pushes.pushes_written += 1;
                self.m_out_bytes = self.m_out_bytes.saturating_add(bytes);
                self.m_out_frames += 1;
            }
            TrySend::Full => self.m_pushes.pushes_dropped_outbound_full += 1,
            TrySend::Closed => self.m_pushes.pushes_dropped_outbound_gone += 1,
        }
        self.maybe_flush_metrics(false);
    }
}
