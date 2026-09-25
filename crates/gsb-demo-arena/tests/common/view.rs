//! The client rules the harness applies to what a connection receives
//! (`kit.proto`, on the arena's typed mirror): a duplicate is discarded,
//! a full replaces the view, a delta with a baseline applies `removed`
//! then the upserts, one without a baseline is dropped (counted).

use gsb_demo_arena::arena::WorldSnapshot;
use gsb_demo_arena::codec::Cm3;

use super::Client;

impl Client {
    /// A group frame under the kit's client rules: a duplicate is
    /// discarded, a full replaces the view, a delta with a baseline
    /// applies `removed` then the upserts, one without is dropped.
    pub(super) fn apply_group(&mut self, s: &WorldSnapshot) {
        if self.baseline.is_some_and(|last| s.sequence <= last) {
            return;
        }
        if !s.delta {
            self.replace(s);
            self.fulls += 1;
        } else if self.baseline.is_none() {
            self.gap_drops += 1;
        } else {
            for id in &s.removed {
                self.view.remove(id);
            }
            self.upsert(s);
            self.baseline = Some(s.sequence);
            self.deltas += 1;
        }
    }

    /// A full: the view is exactly its records, its sequence adopted.
    pub(super) fn replace(&mut self, s: &WorldSnapshot) {
        self.view.clear();
        self.upsert(s);
        self.baseline = Some(s.sequence);
    }

    fn upsert(&mut self, s: &WorldSnapshot) {
        for r in &s.entities {
            let at = Cm3 {
                x: r.x,
                y: r.y,
                z: r.z,
            };
            self.view.insert(r.entity, at);
        }
    }
}
