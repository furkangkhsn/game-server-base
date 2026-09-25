//! The stand-in room / shard logic: its join hook records the identity
//! it was handed, and its wire ids name the shard that minted them.

use gsb_core::id::{ConnectionId, PlayerId};
use gsb_core::room::{Action, Admission, GameLogic, RoomLogic, TickCtx};
use gsb_core::shard::{BorderRecord, Migrating, ShardLogic};
use tokio::sync::mpsc;

/// Wire ids are `shard * SPAN + serial`: the joiner's shard is readable
/// from its JOIN_ROOM_RESULT.
pub const SPAN: u64 = 1_000;
pub const SHARDS: usize = 3;

/// What the router and the join hook saw: `(who, shard, identity)` with
/// `who` = `"route"` (the router; shard = its answer) or `"join"` (the
/// hook of that shard).
pub type Seen = mpsc::UnboundedSender<(&'static str, usize, String)>;

/// A shard (or a single room, index 0) whose join hook records the
/// identity it was handed.
pub struct IdLogic {
    pub index: usize,
    pub serial: u64,
    pub seen: Seen,
}

impl GameLogic<()> for IdLogic {
    type GroupKey = ();
    type Strip = ();
    fn snapshot_op(&self) -> u16 {
        0x7F20
    }
    fn private_op(&self) -> u16 {
        0x7F21
    }
    fn group_of(&self, _w: &(), _p: PlayerId) -> Self::GroupKey {}
    fn snapshot(
        &mut self,
        _w: &mut (),
        _c: &TickCtx,
        _g: &(),
        _b: &[BorderRecord<()>],
        _o: &mut bytes::BytesMut,
    ) -> bool {
        false
    }
    fn on_join(&mut self, _w: &mut (), _c: ConnectionId) -> Admission {
        self.serial += 1;
        let wire = self.index as u64 * SPAN + self.serial;
        Admission {
            player: PlayerId(wire),
            entity: wire,
        }
    }
    fn on_join_as(&mut self, w: &mut (), c: ConnectionId, identity: &str) -> Admission {
        let _ = self.seen.send(("join", self.index, identity.to_string()));
        self.on_join(w, c)
    }
    fn on_leave(&mut self, _w: &mut (), _p: PlayerId) {}
    fn ingest(&mut self, _w: &mut (), _c: &TickCtx, a: &mut Vec<Action>) {
        a.clear();
    }
    fn update(&mut self, _w: &mut (), _c: &TickCtx) {}
}

impl RoomLogic<()> for IdLogic {}

impl ShardLogic<()> for IdLogic {
    type State = ();
    fn index(&self) -> usize {
        self.index
    }
    fn shard_count(&self) -> usize {
        SHARDS
    }
    fn serial_base(&self) -> u64 {
        self.index as u64 * SPAN
    }
    fn serial_range(&self) -> u64 {
        SPAN
    }
    fn serial_used(&self) -> u64 {
        self.serial
    }
    fn neighbors(&self) -> &[usize] {
        &[]
    }
    fn collect_migrations(&mut self, _w: &mut (), _nb: usize) -> Vec<Migrating<()>> {
        Vec::new()
    }
    fn on_migrate_in(&mut self, _w: &mut (), _wire: u64, _s: (), _p: Option<PlayerId>) {}
    fn on_migrate_out(&mut self, _w: &mut (), _wire: u64) {}
    fn collect_border(&self, _w: &()) -> Vec<BorderRecord<()>> {
        Vec::new()
    }
    fn own_wires(&self, _w: &()) -> Vec<u64> {
        Vec::new()
    }
}
