//! A player's session in the actor tests: its out channel applied
//! under the kit's client rules, frame by frame, with the checks the
//! scenarios read (the view after every barrier, doubled wires).

use std::collections::BTreeSet;

use gsb_core::channel::{FrameBatch, Inbox, Mailbox};
use gsb_core::id::{ConnectionId, PlayerId};
use gsb_core::room::Action;
use prost::Message;

use super::front::{Front, MOVE, to};
use crate::client::{ClientDecoder, ClientError, ClientView};
use crate::game::Game;
use crate::testing::{Record, WorldSnapshot};

/// The fixture record decoder (team frames carry no cell exits).
#[derive(Debug, Default)]
pub(super) struct Dec;

impl ClientDecoder for Dec {
    type Record = (i32, i32);
    type Cell = ();

    fn record(&self, body: &[u8]) -> Result<(u64, (i32, i32)), ClientError> {
        let r = Record::decode(body)?;
        Ok((r.entity, (r.x, r.y)))
    }
    fn cell_of(&self, _: &(i32, i32)) {}
    fn cell_exit(&self, _: &[u8]) -> Result<(), ClientError> {
        Err(ClientError::Malformed("a team frame has no cell exit"))
    }
}

/// One player's session.
pub(super) struct Client {
    pub(super) conn: ConnectionId,
    /// Its own wire id.
    pub(super) wire: u64,
    pub(super) out: Inbox<FrameBatch>,
    pub(super) actions: Mailbox<Action>,
    pub(super) view: ClientView<Dec>,
    /// The view's wire ids after each barrier: `(tick, ids)`.
    pub(super) history: Vec<(u64, BTreeSet<u64>)>,
    /// Frames (group or one-shot) that listed one wire id twice.
    pub(super) doubled: u64,
}

impl Client {
    /// Apply everything received so far, frame by frame.
    pub(super) fn drain(&mut self, tick: u64) {
        while let Ok(batch) = self.out.try_recv() {
            for f in batch {
                if f.op == <Front as Game>::SNAPSHOT_OP {
                    let snap = WorldSnapshot::decode(f.payload.as_ref()).expect("snapshot");
                    let ids: BTreeSet<u64> = snap.entities.iter().map(|r| r.entity).collect();
                    if ids.len() != snap.entities.len() {
                        self.doubled += 1;
                    }
                    self.view.apply_snapshot(&f.payload).expect("applies");
                } else if f.op == <Front as Game>::PRIVATE_OP {
                    self.view.apply_private(&f.payload).expect("applies");
                }
            }
        }
        self.history.push((tick, self.view.ids().collect()));
    }

    /// Whether the view holds `wire` now.
    pub(super) fn sees(&self, wire: u64) -> bool {
        self.view.contains(wire)
    }

    /// Teleport this player (applied on the next tick).
    pub(super) fn move_to(&self, x: f32, y: f32) {
        self.actions
            .try_send(Action {
                conn: self.conn,
                player: PlayerId(0),
                op: MOVE,
                payload: to(x, y),
            })
            .expect("action inbox has room");
    }
}
