//! The two minimal logics the shutdown tests drive: a single room and a
//! two-shard grid. Neither does anything per tick; both report a match
//! result, which is how a test observes that a room ran its teardown.

use gsb_core::id::{ConnectionId, PlayerId};
use gsb_core::room::{Action, Admission, GameLogic, RoomLogic, TickCtx};
use gsb_core::shard::{BorderRecord, Migrating, ShardLogic};

/// A single room: one entity per connection, a result at teardown.
pub struct Quiet;

impl GameLogic<()> for Quiet {
    type GroupKey = ();
    type Strip = ();
    fn snapshot_op(&self) -> u16 {
        0x7C00
    }
    fn private_op(&self) -> u16 {
        0x7C01
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
    fn match_result(&mut self, _w: &mut ()) -> Option<bytes::Bytes> {
        Some(bytes::Bytes::from_static(b"single"))
    }
}

impl RoomLogic<()> for Quiet {}

/// One shard of a two-shard grid with no cross-shard traffic (empty
/// topology): every join homes wherever the test's router says.
pub struct QuietShard {
    pub index: usize,
}

impl GameLogic<()> for QuietShard {
    type GroupKey = ();
    type Strip = ();
    fn snapshot_op(&self) -> u16 {
        0x7B00
    }
    fn private_op(&self) -> u16 {
        0x7B01
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
            entity: self.serial_base() + c.0,
        }
    }
    fn on_leave(&mut self, _w: &mut (), _p: PlayerId) {}
    fn ingest(&mut self, _w: &mut (), _c: &TickCtx, a: &mut Vec<Action>) {
        a.clear();
    }
    fn update(&mut self, _w: &mut (), _c: &TickCtx) {}
    fn match_result(&mut self, _w: &mut ()) -> Option<bytes::Bytes> {
        Some(bytes::Bytes::from_static(b"shard"))
    }
}

impl ShardLogic<()> for QuietShard {
    type State = ();
    fn index(&self) -> usize {
        self.index
    }
    fn shard_count(&self) -> usize {
        2
    }
    fn serial_base(&self) -> u64 {
        self.index as u64 * 1000
    }
    fn serial_range(&self) -> u64 {
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
