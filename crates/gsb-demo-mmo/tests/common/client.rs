//! A test client: decodes every frame its connection receives with the
//! MMO's typed mirror and keeps its view under the kit's client rules —
//! a FULL (group or one-shot private) replaces the view; a DELTA with a
//! baseline applies `removed`, then `cell_exits` (every held record in
//! that ground cell is forgotten), then the upserts; a delta without a
//! baseline is dropped; a duplicate sequence is discarded.
//!
//! Stream invariants are asserted on every batch: at most one snapshot
//! and one private frame per tick, and no frame lists an entity twice.

use std::collections::BTreeMap;

use bytes::Bytes;
use gsb_core::PlayerId;
use gsb_core::channel::{FrameBatch, Mailbox};
use gsb_core::id::ConnectionId;
use gsb_core::room::Action;
use gsb_demo_mmo::codec::to_dm;
use gsb_demo_mmo::mmo::{self, EntityRecord, Private, WorldSnapshot, private};
use gsb_demo_mmo::op;
use gsb_demo_mmo::world::client_cell;
use prost::Message;
use tokio::sync::mpsc;

/// One logged-in character's session.
pub struct Client {
    pub conn: ConnectionId,
    /// The character's wire id (the join reply's value).
    pub id: u64,
    rx: mpsc::Receiver<FrameBatch>,
    actions: Mailbox<Action>,
    /// The current view: wire id → record.
    pub view: BTreeMap<u64, EntityRecord>,
    baseline: bool,
    last_seq: u64,
    next_input: u64,
    /// Every input ack received, in order.
    pub acks: Vec<u64>,
    /// Every frame received: `(op, payload)`.
    pub raw: Vec<(u16, Bytes)>,
}

impl Client {
    pub(super) fn new(
        conn: ConnectionId,
        id: u64,
        rx: mpsc::Receiver<FrameBatch>,
        actions: Mailbox<Action>,
    ) -> Self {
        Self {
            conn,
            id,
            rx,
            actions,
            view: BTreeMap::new(),
            baseline: false,
            last_seq: 0,
            next_input: 1,
            acks: Vec::new(),
            raw: Vec::new(),
        }
    }

    /// The record of wire id `id` in the view.
    pub fn get(&self, id: u64) -> Option<&EntityRecord> {
        self.view.get(&id)
    }

    /// This client's own record.
    pub fn me(&self) -> Option<&EntityRecord> {
        self.get(self.id)
    }

    /// The wire ids in the view, sorted.
    pub fn sees(&self) -> Vec<u64> {
        self.view.keys().copied().collect()
    }

    /// The records of `kind` in the view.
    pub fn of_kind(&self, kind: mmo::Kind) -> Vec<(u64, EntityRecord)> {
        self.view
            .iter()
            .filter(|(_, r)| r.kind == kind as i32)
            .map(|(id, r)| (*id, *r))
            .collect()
    }

    async fn send(&mut self, op: u16, payload: Vec<u8>) {
        self.actions
            .send(Action {
                // The core stamps the stable player id from its binding.
                player: PlayerId(self.conn.0),
                conn: self.conn,
                op,
                payload: Bytes::from(payload),
            })
            .await
            .expect("action channel alive");
    }

    fn seq(&mut self) -> u64 {
        self.next_input += 1;
        self.next_input - 1
    }

    /// Walk toward `(x, z)` metres (a numbered input).
    pub async fn move_to(&mut self, x: f32, z: f32) {
        let seq = self.seq();
        let m = mmo::MoveTo {
            x: to_dm(x),
            z: to_dm(z),
            seq,
        };
        self.send(op::MMO_MOVE_TO, m.encode_to_vec()).await;
    }

    /// Hit the mob with wire id `target`.
    pub async fn attack(&mut self, target: u64) {
        let seq = self.seq();
        let m = mmo::Attack { target, seq };
        self.send(op::MMO_ATTACK, m.encode_to_vec()).await;
    }

    /// Use waystone `waystone`.
    pub async fn travel(&mut self, waystone: u32) {
        let seq = self.seq();
        let m = mmo::Travel { waystone, seq };
        self.send(op::MMO_TRAVEL, m.encode_to_vec()).await;
    }

    /// Decode everything received since the last call.
    pub(super) fn drain(&mut self) {
        while let Ok(batch) = self.rx.try_recv() {
            let snaps = batch.iter().filter(|f| f.op == op::MMO_SNAPSHOT).count();
            let privs = batch.iter().filter(|f| f.op == op::MMO_PRIVATE).count();
            assert!(
                snaps <= 1 && privs <= 1,
                "one snapshot + one private per tick"
            );
            for f in batch.iter() {
                self.raw.push((f.op, f.payload.clone()));
                match f.op {
                    op::MMO_SNAPSHOT => {
                        let s = WorldSnapshot::decode(f.payload.as_ref()).expect("snapshot");
                        self.apply_group(s);
                    }
                    op::MMO_PRIVATE => {
                        let p = Private::decode(f.payload.as_ref()).expect("private");
                        match p.payload {
                            Some(private::Payload::Ack(a)) => self.acks.push(a.processed_up_to),
                            Some(private::Payload::Snapshot(s)) => {
                                assert!(!s.delta, "the one-shot private view is a full");
                                self.replace(s);
                            }
                            None => {}
                        }
                    }
                    other => panic!("unexpected frame op {other}"),
                }
            }
        }
    }

    fn replace(&mut self, s: WorldSnapshot) {
        assert_unique(&s);
        self.view = s.entities.iter().map(|r| (r.entity, *r)).collect();
        self.baseline = true;
        self.last_seq = s.sequence;
    }

    fn apply_group(&mut self, s: WorldSnapshot) {
        if s.sequence <= self.last_seq && self.baseline {
            return; // a duplicate: discarded
        }
        if !s.delta {
            return self.replace(s);
        }
        if !self.baseline {
            return; // no baseline: dropped until a full
        }
        assert_unique(&s);
        for id in &s.removed {
            self.view.remove(id);
        }
        for exit in &s.cell_exits {
            self.view
                .retain(|_, r| client_cell(r.x, r.z) != (exit.x, exit.z));
        }
        for r in &s.entities {
            self.view.insert(r.entity, *r);
        }
        self.last_seq = s.sequence;
    }
}

fn assert_unique(s: &WorldSnapshot) {
    let mut ids: Vec<u64> = s.entities.iter().map(|r| r.entity).collect();
    ids.sort_unstable();
    let n = ids.len();
    ids.dedup();
    assert_eq!(ids.len(), n, "a frame listed an entity twice: {s:?}");
}
