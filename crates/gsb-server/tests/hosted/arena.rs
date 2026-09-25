//! The arena client's view, under the kit's client rules (`kit.proto`):
//! the arena's team room ships deltas — a full replaces the view, a
//! delta with a baseline applies `removed` then the upserts, one without
//! a baseline is dropped, a duplicate is discarded; a one-shot private
//! full replaces the view. The private frame also carries the input ack.

use std::collections::BTreeMap;

use gsb_demo_arena::arena::{MoveTo, Private, WorldSnapshot, private};
use gsb_demo_arena::codec::{Cm3, to_cm};
use gsb_demo_arena::op;
use prost::Message;

use super::{Client, View};

/// What an arena client holds.
#[derive(Default)]
pub struct ArenaView {
    /// The team view: wire id → position (centimetres).
    pub units: BTreeMap<u64, Cm3>,
    /// Every input ack, in order.
    pub acks: Vec<u64>,
    /// Snapshots applied so far (group frames and one-shot fulls).
    pub snapshots: u64,
    /// The last accepted sequence (`None`: no baseline yet).
    baseline: Option<u64>,
}

impl ArenaView {
    /// A full: the view is exactly its records, its sequence adopted.
    fn replace(&mut self, s: &WorldSnapshot) {
        self.units.clear();
        self.upsert(s);
        self.baseline = Some(s.sequence);
        self.snapshots += 1;
    }

    fn upsert(&mut self, s: &WorldSnapshot) {
        for r in &s.entities {
            let at = Cm3 {
                x: r.x,
                y: r.y,
                z: r.z,
            };
            self.units.insert(r.entity, at);
        }
    }
}

impl View for ArenaView {
    fn apply(&mut self, code: u16, payload: &[u8]) {
        match code {
            op::ARENA_SNAPSHOT => {
                let s = WorldSnapshot::decode(payload).expect("arena snapshot");
                if self.baseline.is_some_and(|last| s.sequence <= last) {
                    return; // a duplicate
                }
                if !s.delta {
                    self.replace(&s);
                } else if self.baseline.is_some() {
                    for id in &s.removed {
                        self.units.remove(id);
                    }
                    self.upsert(&s);
                    self.baseline = Some(s.sequence);
                    self.snapshots += 1;
                } // else: no baseline yet — dropped until a full
            }
            op::ARENA_PRIVATE => {
                let p = Private::decode(payload).expect("arena private");
                match p.payload {
                    Some(private::Payload::Ack(a)) => self.acks.push(a.processed_up_to),
                    Some(private::Payload::Snapshot(s)) => {
                        assert!(!s.delta, "a one-shot private snapshot is a full");
                        self.replace(&s);
                    }
                    None => {}
                }
            }
            other => panic!("unexpected arena frame op {other}"),
        }
    }
}

impl ArenaView {
    /// The wire ids in view, sorted.
    pub fn sees(&self) -> Vec<u64> {
        self.units.keys().copied().collect()
    }
}

impl Client<ArenaView> {
    /// Send a `MoveTo` (metres; `seq` 0 = unnumbered).
    pub async fn move_to(&mut self, x: f32, y: f32, z: f32, seq: u64) {
        let m = MoveTo {
            x: to_cm(x),
            y: to_cm(y),
            z: to_cm(z),
            seq,
        };
        self.send(op::ARENA_MOVE_TO, &m.encode_to_vec()).await;
    }

    /// This client's own unit, if in view.
    pub fn me(&self) -> Option<Cm3> {
        self.view.units.get(&self.entity).copied()
    }
}

/// 3D distance between two records, centimetres.
pub fn dist_cm(a: Cm3, b: Cm3) -> f64 {
    let d = |p: i32, q: i32| f64::from(p - q).powi(2);
    (d(a.x, b.x) + d(a.y, b.y) + d(a.z, b.z)).sqrt()
}
