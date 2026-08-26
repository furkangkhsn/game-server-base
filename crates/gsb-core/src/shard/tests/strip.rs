//! Per-link exchange modes and a richer strip payload: the local
//! link stays always-full, opposite directions may differ, and a
//! multi-field strip survives both packagings.

use super::*;

mod modes;

/// A strip payload with one field BEYOND position (a facing-like
/// quantity; the combat/prediction shape this generalization exists
/// for).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TRich {
    x: i32,
    y: i32,
    facing: i16,
}

/// A minimal shard logic whose strip carries [`TRich`]. The payload is
/// assembled from game state (`TWorld`'s third slot read as facing) —
/// exactly the ownership split under test: the core could never have
/// derived this field. `update` is a no-op, so entities stay put and
/// only an explicit mutation changes anything.
struct RichLogic {
    index: usize,
}

impl GameLogic<TWorld> for RichLogic {
    type GroupKey = ();
    type Strip = TRich;

    fn snapshot_op(&self) -> u16 {
        0x7300
    }
    fn private_op(&self) -> u16 {
        0x7301
    }
    fn group_of(&self, _w: &TWorld, _p: PlayerId) -> Self::GroupKey {}
    fn snapshot(
        &mut self,
        _w: &mut TWorld,
        _c: &TickCtx,
        _g: &Self::GroupKey,
        _borrowed: &[BorderRecord<TRich>],
        _out: &mut bytes::BytesMut,
    ) -> bool {
        false // no members join in these tests; nothing ever emits
    }
    fn on_join(&mut self, w: &mut TWorld, conn: ConnectionId) -> Admission {
        let wire = self.index as u64 * SHARD_SERIAL_RANGE + conn.0;
        w.ents.insert(wire, ((conn.0 % 20) as f32 - 10.0, 0.0, 0));
        Admission {
            player: PlayerId(conn.0),
            entity: wire,
        }
    }
    fn on_leave(&mut self, _w: &mut TWorld, _player: PlayerId) {}
    fn ingest(&mut self, _w: &mut TWorld, _c: &TickCtx, actions: &mut Vec<Action>) {
        actions.clear();
    }
    fn update(&mut self, _w: &mut TWorld, _c: &TickCtx) {}
}

impl ShardLogic<TWorld> for RichLogic {
    type State = TState;

    fn index(&self) -> usize {
        self.index
    }
    fn shard_count(&self) -> usize {
        2
    }
    fn serial_base(&self) -> u64 {
        self.index as u64 * SHARD_SERIAL_RANGE
    }
    fn serial_range(&self) -> u64 {
        SHARD_SERIAL_RANGE
    }
    fn serial_used(&self) -> u64 {
        0
    }
    fn neighbors(&self) -> &[usize] {
        if self.index == 0 {
            &[1]
        } else {
            &[0]
        }
    }
    fn collect_migrations(
        &mut self,
        _w: &mut TWorld,
        _nb: usize,
    ) -> Vec<Migrating<TState>> {
        Vec::new()
    }
    fn on_migrate_in(
        &mut self,
        _w: &mut TWorld,
        _wire: u64,
        _state: TState,
        _player: Option<PlayerId>,
    ) {
    }
    fn on_migrate_out(&mut self, _w: &mut TWorld, _wire: u64) {}
    fn collect_border(&self, w: &TWorld) -> Vec<BorderRecord<TRich>> {
        // The strip: same |x| <= 1 frame as TLogic, plus the CUSTOM
        // field from game state.
        w.ents
            .iter()
            .filter(|(_, (x, _, _))| x.abs() <= 1.0)
            .map(|(wire, (x, y, facing))| BorderRecord {
                wire: *wire,
                state: TRich {
                    x: *x as i32,
                    y: *y as i32,
                    facing: *facing as i16,
                },
            })
            .collect()
    }
    fn own_wires(&self, w: &TWorld) -> Vec<u64> {
        w.ents.keys().copied().collect()
    }
}
