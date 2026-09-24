//! The MMO client's view under the kit's client rules (written out in
//! gsb-kit's `kit.proto`): a FULL — group snapshot or one-shot private —
//! replaces the view; a DELTA with a baseline applies `removed`, then
//! `cell_exits` (every held record in that ground cell is forgotten),
//! then the upserts; a delta without a baseline is dropped; a stale
//! sequence is discarded. (The MMO's own test client applies the same
//! rules to in-process channels; GAME-MODULE G4 folds the copies into
//! one kit client.)

use std::collections::BTreeMap;

use gsb_demo_mmo::codec::to_dm;
use gsb_demo_mmo::mmo::{self, EntityRecord, Private, WorldSnapshot, private};
use gsb_demo_mmo::op;
use gsb_demo_mmo::world::client_cell;
use prost::Message;

use super::{Client, View};

/// What an MMO client holds.
#[derive(Default)]
pub struct MmoView {
    /// The current view: wire id → record.
    pub records: BTreeMap<u64, EntityRecord>,
    baseline: bool,
    last_seq: u64,
    /// Every input ack, in order.
    pub acks: Vec<u64>,
    /// Frames that changed the view (full or applied delta).
    pub applied: u64,
    /// An id the view must keep once it has seen it: every applied frame
    /// after which it is missing counts in [`Self::lost`].
    pub watch: Option<u64>,
    watched: bool,
    /// Applied frames after which the watched id was missing.
    pub lost: u64,
}

impl View for MmoView {
    fn apply(&mut self, code: u16, payload: &[u8]) {
        match code {
            op::MMO_SNAPSHOT => {
                let s = WorldSnapshot::decode(payload).expect("mmo snapshot");
                self.apply_group(s);
            }
            op::MMO_PRIVATE => {
                let p = Private::decode(payload).expect("mmo private");
                match p.payload {
                    Some(private::Payload::Ack(a)) => self.acks.push(a.processed_up_to),
                    Some(private::Payload::Snapshot(s)) => {
                        assert!(!s.delta, "the one-shot private view is a full");
                        self.replace(s);
                    }
                    None => {}
                }
            }
            other => panic!("unexpected MMO frame op {other}"),
        }
    }
}

impl MmoView {
    fn replace(&mut self, s: WorldSnapshot) {
        self.records = s.entities.iter().map(|r| (r.entity, *r)).collect();
        self.baseline = true;
        self.last_seq = s.sequence;
        self.book();
    }

    /// Book one applied frame (and the watched id's presence after it).
    fn book(&mut self) {
        self.applied += 1;
        if let Some(id) = self.watch {
            if self.records.contains_key(&id) {
                self.watched = true;
            } else if self.watched {
                self.lost += 1;
            }
        }
    }

    fn apply_group(&mut self, s: WorldSnapshot) {
        if self.baseline && s.sequence <= self.last_seq {
            return; // stale: discarded
        }
        if !s.delta {
            return self.replace(s);
        }
        if !self.baseline {
            return; // no baseline yet: dropped until a full
        }
        for id in &s.removed {
            self.records.remove(id);
        }
        for exit in &s.cell_exits {
            self.records
                .retain(|_, r| client_cell(r.x, r.z) != (exit.x, exit.z));
        }
        for r in &s.entities {
            self.records.insert(r.entity, *r);
        }
        self.last_seq = s.sequence;
        self.book();
    }

    /// Whether the view has had its first full.
    pub fn has_baseline(&self) -> bool {
        self.baseline
    }

    /// The wire ids of the players in view, sorted.
    pub fn players(&self) -> Vec<u64> {
        self.of_kind(mmo::Kind::Player)
    }

    /// The wire ids of `kind` in view, sorted.
    pub fn of_kind(&self, kind: mmo::Kind) -> Vec<u64> {
        self.records
            .values()
            .filter(|r| r.kind == kind as i32)
            .map(|r| r.entity)
            .collect()
    }
}

impl Client<MmoView> {
    /// This client's own record, if in view.
    pub fn me(&self) -> Option<EntityRecord> {
        self.view.records.get(&self.entity).copied()
    }

    /// The record of `id`, if in view.
    pub fn sees(&self, id: u64) -> Option<EntityRecord> {
        self.view.records.get(&id).copied()
    }

    /// Walk toward `(x, z)` metres.
    pub async fn move_to(&mut self, x: f32, z: f32, seq: u64) {
        let m = mmo::MoveTo {
            x: to_dm(x),
            z: to_dm(z),
            seq,
        };
        self.send(op::MMO_MOVE_TO, &m.encode_to_vec()).await;
    }

    /// Hit the mob with wire id `target`.
    pub async fn attack(&mut self, target: u64, seq: u64) {
        let m = mmo::Attack { target, seq };
        self.send(op::MMO_ATTACK, &m.encode_to_vec()).await;
    }

    /// Use waystone `waystone`.
    pub async fn travel(&mut self, waystone: u32, seq: u64) {
        let m = mmo::Travel { waystone, seq };
        self.send(op::MMO_TRAVEL, &m.encode_to_vec()).await;
    }
}

/// `(x, z)` metres as the wire's decimetres (what a record carries).
pub fn dm(x: f32, z: f32) -> (i32, i32) {
    (to_dm(x), to_dm(z))
}

/// A record's ground position in decimetres.
pub fn ground(r: &EntityRecord) -> (i32, i32) {
    (r.x, r.z)
}
