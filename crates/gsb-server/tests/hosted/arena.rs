//! The arena client's view: the team room sends FULL snapshots only, so
//! each snapshot replaces the view; the private frame carries the input
//! ack.

use std::collections::BTreeMap;

use gsb_demo_arena::arena::{MoveTo, Private, WorldSnapshot, private};
use gsb_demo_arena::codec::{Cm3, to_cm};
use gsb_demo_arena::op;
use prost::Message;

use super::{Client, View};

/// What an arena client holds.
#[derive(Default)]
pub struct ArenaView {
    /// The latest team snapshot: wire id → position (centimetres).
    pub units: BTreeMap<u64, Cm3>,
    /// Every input ack, in order.
    pub acks: Vec<u64>,
    /// Snapshots applied so far.
    pub snapshots: u64,
}

impl View for ArenaView {
    fn apply(&mut self, code: u16, payload: &[u8]) {
        match code {
            op::ARENA_SNAPSHOT => {
                let s = WorldSnapshot::decode(payload).expect("arena snapshot");
                assert!(!s.delta, "the team-fog room sends full snapshots");
                self.units = s
                    .entities
                    .iter()
                    .map(|r| {
                        let at = Cm3 {
                            x: r.x,
                            y: r.y,
                            z: r.z,
                        };
                        (r.entity, at)
                    })
                    .collect();
                self.snapshots += 1;
            }
            op::ARENA_PRIVATE => {
                let p = Private::decode(payload).expect("arena private");
                if let Some(private::Payload::Ack(a)) = p.payload {
                    self.acks.push(a.processed_up_to);
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
