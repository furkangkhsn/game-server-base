//! A player's session in the actor tests: its out channel applied under
//! the kit's client rules (`gsb_kit::client` over the game's decode
//! seam), frame by frame, with the checks the scenarios read.
//!
//! Stream invariants are asserted on every batch: at most one snapshot
//! and one private frame per tick, and no frame lists a wire id twice
//! (checked on the game's typed mirror).

use std::collections::BTreeSet;

use bytes::Bytes;
use gsb_core::PlayerId;
use gsb_core::channel::{FrameBatch, Inbox, Mailbox};
use gsb_core::id::ConnectionId;
use gsb_core::room::Action;
use gsb_demo_war::codec::to_dm;
use gsb_demo_war::op;
use gsb_demo_war::war::{self, Private, UnitRecord, Welcome, WorldSnapshot, private};
use gsb_kit::client::{ClientDecoder, ClientError, ClientView, PrivateEvent};
use prost::Message;

/// The game's decode seam: a record is kept whole; team frames carry no
/// cell exits; the session payload is the `Welcome`, kept.
#[derive(Default)]
pub struct WarDecoder {
    /// Every welcome received, in order.
    pub welcomes: Vec<Welcome>,
}

impl ClientDecoder for WarDecoder {
    type Record = UnitRecord;
    type Cell = ();

    fn record(&self, body: &[u8]) -> Result<(u64, UnitRecord), ClientError> {
        let r = UnitRecord::decode(body)?;
        Ok((r.entity, r))
    }

    fn cell_of(&self, _: &UnitRecord) {}

    fn cell_exit(&self, _: &[u8]) -> Result<(), ClientError> {
        Err(ClientError::Malformed("a team frame carries no cell exit"))
    }

    fn session_private(&mut self, body: &[u8]) -> Result<(), ClientError> {
        self.welcomes.push(Welcome::decode(body)?);
        Ok(())
    }
}

/// One player's session.
pub struct Client {
    pub conn: ConnectionId,
    /// Its own wire id (the join reply's value).
    pub wire: u64,
    out: Inbox<FrameBatch>,
    actions: Mailbox<Action>,
    pub view: ClientView<WarDecoder>,
    next_input: u64,
    /// The view's wire ids after each barrier: `(tick, ids)`.
    pub history: Vec<(u64, BTreeSet<u64>)>,
    /// Every input ack received, in order.
    pub acks: Vec<u64>,
    /// Every frame received: `(op, payload)`.
    pub raw: Vec<(u16, Bytes)>,
}

impl Client {
    pub(super) fn new(
        conn: ConnectionId,
        wire: u64,
        out: Inbox<FrameBatch>,
        actions: Mailbox<Action>,
    ) -> Self {
        Self {
            conn,
            wire,
            out,
            actions,
            view: ClientView::default(),
            next_input: 1,
            history: Vec::new(),
            acks: Vec::new(),
            raw: Vec::new(),
        }
    }

    /// Whether the view holds `wire` now.
    pub fn sees(&self, wire: u64) -> bool {
        self.view.contains(wire)
    }

    /// The record of `wire` in the view.
    pub fn get(&self, wire: u64) -> Option<&UnitRecord> {
        self.view.get(wire)
    }

    /// This client's own record.
    pub fn me(&self) -> Option<&UnitRecord> {
        self.get(self.wire)
    }

    /// The records in view of `kind`.
    pub fn of_kind(&self, kind: war::Kind) -> Vec<UnitRecord> {
        let mut v: Vec<UnitRecord> = self
            .view
            .values()
            .filter(|r| r.kind == kind as i32)
            .copied()
            .collect();
        v.sort_unstable_by_key(|r| r.entity);
        v
    }

    /// The session's welcomes.
    pub fn welcomes(&self) -> &[Welcome] {
        &self.view.decoder().welcomes
    }

    fn send(&mut self, op: u16, payload: Vec<u8>) {
        self.actions
            .try_send(Action {
                // The core stamps the stable player id from its binding.
                player: PlayerId(self.conn.0),
                conn: self.conn,
                op,
                payload: Bytes::from(payload),
            })
            .expect("action inbox has room");
    }

    fn seq(&mut self) -> u64 {
        self.next_input += 1;
        self.next_input - 1
    }

    /// Walk toward `(x, z)` metres (a numbered input; applied next tick).
    pub fn move_to(&mut self, x: f32, z: f32) {
        let seq = self.seq();
        let m = war::MoveTo {
            x: to_dm(x),
            z: to_dm(z),
            seq,
        };
        self.send(op::WAR_MOVE_TO, m.encode_to_vec());
    }

    /// Hit the unit with wire id `target` (a numbered input).
    pub fn attack(&mut self, target: u64) {
        let seq = self.seq();
        let m = war::Attack { target, seq };
        self.send(op::WAR_ATTACK, m.encode_to_vec());
    }

    /// Apply everything received so far, frame by frame.
    pub(super) fn drain(&mut self, tick: u64) {
        while let Ok(batch) = self.out.try_recv() {
            let snaps = batch.iter().filter(|f| f.op == op::WAR_SNAPSHOT).count();
            let privs = batch.iter().filter(|f| f.op == op::WAR_PRIVATE).count();
            assert!(
                snaps <= 1 && privs <= 1,
                "one snapshot + one private per tick"
            );
            for f in batch.iter() {
                self.raw.push((f.op, f.payload.clone()));
                match f.op {
                    op::WAR_SNAPSHOT => {
                        let s = WorldSnapshot::decode(f.payload.as_ref()).expect("snapshot");
                        assert_unique(&s);
                        self.view
                            .apply_snapshot(f.payload.as_ref())
                            .expect("applies");
                    }
                    op::WAR_PRIVATE => {
                        let p = Private::decode(f.payload.as_ref()).expect("private");
                        if let Some(private::Payload::Snapshot(s)) = &p.payload {
                            assert_unique(s);
                        }
                        match self.view.apply_private(f.payload.as_ref()) {
                            Ok(PrivateEvent::Ack(up_to)) => self.acks.push(up_to),
                            Ok(_) => {}
                            Err(e) => panic!("private: {e}"),
                        }
                    }
                    other => panic!("unexpected frame op {other}"),
                }
            }
        }
        self.history.push((tick, self.view.ids().collect()));
    }
}

fn assert_unique(s: &WorldSnapshot) {
    let ids: BTreeSet<u64> = s.entities.iter().map(|r| r.entity).collect();
    assert_eq!(
        ids.len(),
        s.entities.len(),
        "a frame listed a unit twice: {s:?}"
    );
}
