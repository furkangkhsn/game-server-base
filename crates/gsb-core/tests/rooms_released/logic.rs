//! Two do-nothing logics whose teardown is SLOW: `on_shutdown` blocks for
//! a while, then reports itself on a channel.

use std::time::Duration;

use gsb_core::id::{ConnectionId, PlayerId};
use gsb_core::registry::BuiltRoom;
use gsb_core::room::{Action, Admission, GameLogic, RoomLogic, TickCtx};
use gsb_core::shard::{BorderRecord, Migrating, ShardLogic};
use tokio::sync::mpsc;

/// How long each teardown blocks (a room's tick body is synchronous; a
/// long final settlement is the case the barrier exists for).
const TEARDOWN: Duration = Duration::from_millis(150);

/// Which actor finished its teardown.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Torn {
    Room,
    Shard(usize),
}

fn tear(tx: &mpsc::Sender<Torn>, who: Torn) {
    std::thread::sleep(TEARDOWN);
    let _ = tx.try_send(who);
}

/// The shared no-op half of both logics.
macro_rules! quiet_logic {
    () => {
        type GroupKey = ();
        type Strip = ();
        fn snapshot_op(&self) -> u16 {
            0x7A00
        }
        fn private_op(&self) -> u16 {
            0x7A01
        }
        fn group_of(&self, _w: &(), _p: PlayerId) -> Self::GroupKey {}
        fn snapshot(
            &mut self,
            _w: &mut (),
            _c: &TickCtx,
            _g: &(),
            _borrowed: &[BorderRecord<()>],
            _o: &mut bytes::BytesMut,
        ) -> bool {
            false
        }
        fn on_join(&mut self, _w: &mut (), c: ConnectionId) -> Admission {
            Admission {
                player: PlayerId(c.0),
                entity: c.0,
            }
        }
        fn on_leave(&mut self, _w: &mut (), _p: PlayerId) {}
        fn ingest(&mut self, _w: &mut (), _c: &TickCtx, a: &mut Vec<Action>) {
            a.clear();
        }
        fn update(&mut self, _w: &mut (), _c: &TickCtx) {}
    };
}

/// A single room with a slow teardown.
pub struct Slow {
    torn: mpsc::Sender<Torn>,
}

impl Slow {
    pub fn new(torn: mpsc::Sender<Torn>) -> Self {
        Self { torn }
    }
}

impl GameLogic<()> for Slow {
    quiet_logic!();
    fn on_shutdown(&mut self) {
        tear(&self.torn, Torn::Room);
    }
}

impl RoomLogic<()> for Slow {}

/// One shard of a two-shard grid with a slow teardown.
pub struct SlowShard {
    index: usize,
    torn: mpsc::Sender<Torn>,
}

impl SlowShard {
    pub fn new(index: usize, torn: mpsc::Sender<Torn>) -> Self {
        Self { index, torn }
    }
}

impl GameLogic<()> for SlowShard {
    quiet_logic!();
    fn on_shutdown(&mut self) {
        tear(&self.torn, Torn::Shard(self.index));
    }
}

impl ShardLogic<()> for SlowShard {
    type State = ();
    fn index(&self) -> usize {
        self.index
    }
    fn shard_count(&self) -> usize {
        2
    }
    fn serial_capacity(&self) -> u64 {
        1000
    }
    fn serial_used(&self) -> u64 {
        0
    }
    fn neighbors(&self) -> &[usize] {
        &[]
    }
    fn collect_migrations(&mut self, _w: &mut (), _neighbor: usize) -> Vec<Migrating<()>> {
        Vec::new()
    }
    fn on_migrate_in(&mut self, _w: &mut (), _wire: u64, _state: (), _player: Option<PlayerId>) {}
    fn on_migrate_out(&mut self, _w: &mut (), _wire: u64) {}
    fn collect_border(&self, _w: &()) -> Vec<BorderRecord<()>> {
        Vec::new()
    }
    fn own_wires(&self, _w: &()) -> Vec<u64> {
        Vec::new()
    }
}

/// A single room built from `logic`.
pub fn single(logic: Slow) -> BuiltRoom<(), (), (), ()> {
    BuiltRoom::Single {
        world: (),
        logic: Box::new(logic) as Box<dyn RoomLogic<(), GroupKey = (), Strip = ()>>,
    }
}

/// One shard of a grid built from `logic`.
pub fn shard(logic: SlowShard) -> gsb_core::registry::Shard<(), (), (), ()> {
    (
        (),
        Box::new(logic) as Box<dyn ShardLogic<(), GroupKey = (), State = (), Strip = ()>>,
    )
}
