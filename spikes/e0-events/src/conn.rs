//! The connection actor's new arm (docs/EVENTS.md §6.2): the CLIENT leg
//! arrives in its inbox (`ConnIn::Push` in the engine) and leaves as a
//! `PUSH` frame on the connection's own outbound — the same channel the
//! room's broadcast and the actor's control answers use. No room is on
//! this path, so a chat line costs no room anything.
//!
//! The router shares the body (`Bytes`, reference counted); the frame
//! here is its small header plus a copy of the body — the per-recipient
//! work happens in N connection actors, not in the publisher or router.

use bytes::Bytes;
use tokio::sync::mpsc::{self, error::TrySendError};

use crate::contract::{ConnId, PublisherId, WireMessage};
use crate::wire::{Frame, PUSH, Push};

/// The connection actor's inbox (only the arm this round adds).
#[derive(Debug)]
pub enum ConnMsg {
    /// One CLIENT leg.
    Push {
        /// Who published.
        publisher: PublisherId,
        /// The CLIENT-leg sequence number.
        seq: u64,
        /// The event's op.
        op: u16,
        /// The shared body.
        body: Bytes,
    },
}

/// The connection's counters for this path.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ConnCounts {
    /// PUSH frames queued on the outbound.
    pub pushes_written: u64,
    /// Dropped: the outbound (writer) channel full — a slow client.
    pub pushes_dropped_outbound_full: u64,
    /// Dropped: the writer gone (the session is ending).
    pub pushes_dropped_outbound_gone: u64,
}

/// A connection actor reduced to the push arm.
pub struct ConnActor {
    /// The session.
    pub id: ConnId,
    rx: mpsc::Receiver<ConnMsg>,
    out: mpsc::Sender<Frame>,
    counts: ConnCounts,
    /// The bodies' addresses as received (tests: shared, not copied).
    pub seen_bodies: Vec<usize>,
}

impl ConnActor {
    /// An actor with an inbox of `cap`, writing to `out`.
    pub fn new(id: ConnId, cap: usize, out: mpsc::Sender<Frame>) -> (Self, mpsc::Sender<ConnMsg>) {
        let (tx, rx) = mpsc::channel(cap);
        (
            Self {
                id,
                rx,
                out,
                counts: ConnCounts::default(),
                seen_bodies: Vec::new(),
            },
            tx,
        )
    }

    /// Drain the inbox (the actor's loop would `recv().await` one at a
    /// time; the arm is the same).
    pub fn pump(&mut self) {
        while let Ok(m) = self.rx.try_recv() {
            self.handle(m);
        }
    }

    fn handle(&mut self, m: ConnMsg) {
        match m {
            ConnMsg::Push { publisher, seq, op, body } => {
                self.seen_bodies.push(body.as_ptr() as usize);
                let frame = Frame {
                    op: PUSH,
                    payload: Push {
                        publisher: publisher.0,
                        seq,
                        op: op as u32,
                        body,
                    }
                    .to_bytes(),
                };
                match self.out.try_send(frame) {
                    Ok(()) => self.counts.pushes_written += 1,
                    Err(TrySendError::Full(_)) => self.counts.pushes_dropped_outbound_full += 1,
                    Err(TrySendError::Closed(_)) => self.counts.pushes_dropped_outbound_gone += 1,
                }
            }
        }
    }

    /// The counters.
    pub fn counts(&self) -> &ConnCounts {
        &self.counts
    }
}
