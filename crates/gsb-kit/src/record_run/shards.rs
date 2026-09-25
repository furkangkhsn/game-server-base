//! The sharded twin, driven in process: four shard logics per room
//! stepped by hand in the core's phase order — migrations of the last
//! tick land (the source despawns, the target installs, the player
//! follows), inputs, update, this tick's crossings, the border strips
//! (each shard's borrowed view: every other shard's strip, own wires
//! filtered out), the TEAMS phase (each shard imports what the others
//! exported the tick before), then the broadcast (group frames, the
//! keep-alive on the cadence, private frames).
//!
//! Why not the actor rig: shard actors exchange strips and hand off
//! migrations over channels, and which of two same-tick messages a
//! shard sees first is scheduling (the accepted one-tick staleness at a
//! seam, `gsb_core::shard` module docs) — two actor rooms need not ship
//! the same bytes. Stepped by hand, both rooms see exactly the same
//! order, so their frames must carry the same records.

use std::collections::BTreeMap;

use super::CELL;
use super::compare::{Stats, add, same};
use super::game::{MOVE, SWITCH, to};
use super::layout::take;
use super::script::Op;
use crate::client::{ClientView, Counters};
use crate::testing::Dec;

use room::{Logic, Room};

mod room;

/// A group key, as the core bounds it.
pub(super) trait Key:
    Eq + std::hash::Hash + Clone + std::fmt::Debug + Send + 'static
{
}
impl<T: Eq + std::hash::Hash + Clone + std::fmt::Debug + Send + 'static> Key for T {}
/// A migration state, as the core bounds it.
pub(super) trait Mig: std::fmt::Debug + Send + 'static {}
impl<T: std::fmt::Debug + Send + 'static> Mig for T {}

/// The twin: room 0 over the `entities` codec, room 1 over the run.
pub(super) struct Shards<G, St> {
    rooms: [Room<G, St>; 2],
    views: Vec<(ClientView<Dec<false>>, ClientView<Dec<true>>)>,
    next_conn: u64,
    tick: u64,
    pub(super) stats: (Stats, Stats),
}

impl<G: Key, St: Mig> Shards<G, St> {
    pub(super) fn new(build: impl Fn(bool, usize) -> Logic<G, St>, shards: usize) -> Self {
        let room = |run| Room::new((0..shards).map(|i| build(run, i)).collect());
        Self {
            rooms: [room(false), room(true)],
            views: Vec::new(),
            next_conn: 1,
            tick: 0,
            stats: (Stats::default(), Stats::default()),
        }
    }

    /// Apply one tick's operations to both rooms, step both, deliver
    /// every batch and compare the pairs.
    pub(super) fn play(&mut self, ops: &[Op]) {
        for op in ops {
            for room in &mut self.rooms {
                match op {
                    Op::Join(identity) => room.join(self.next_conn, identity),
                    Op::Leave(i) => room.leave(*i),
                    Op::Move(i, x, y) => room.input(*i, MOVE, to(*x, *y)),
                    Op::Switch(i, team) => room.input(*i, SWITCH, vec![*team].into()),
                }
            }
            match op {
                Op::Join(_) => {
                    self.next_conn += 1;
                    let (a, b) = (Dec::new(CELL), Dec::new(CELL));
                    self.views.push((ClientView::new(a), ClientView::new(b)));
                }
                Op::Leave(i) => drop(self.views.remove(*i)),
                _ => {}
            }
        }
        self.tick += 1;
        let tick = self.tick;
        let sent = [self.rooms[0].step(tick), self.rooms[1].step(tick)];
        for (i, (a, b)) in self.views.iter_mut().enumerate() {
            let (mut fa, mut fb) = (Vec::new(), Vec::new());
            for (snapshot, frame) in self.rooms[0].batch(&sent[0], i) {
                take(a, &mut fa, snapshot, &frame);
            }
            for (snapshot, frame) in self.rooms[1].batch(&sent[1], i) {
                take(b, &mut fb, snapshot, &frame);
            }
            self.stats.0.see(&fa);
            self.stats.1.see(&fb);
            same(tick, (a, &fa), (b, &fb));
        }
    }

    /// Every run-side client's counters, summed.
    pub(super) fn counters(&self) -> Counters {
        let mut sum = Counters::default();
        for (_, b) in &self.views {
            add(&mut sum, b.counters());
        }
        sum
    }

    /// Where the live players stand, per shard (the run room's).
    pub(super) fn spread(&self) -> BTreeMap<usize, usize> {
        let mut at = BTreeMap::new();
        for &(_, s) in &self.rooms[1].players {
            *at.entry(s).or_default() += 1;
        }
        at
    }
}
