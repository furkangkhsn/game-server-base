//! The TEAMS phase tests' logic: one group, no world of its own, a
//! scripted team exchange.

use std::collections::VecDeque;

use super::*;

/// What the stub logic saw of the imports: `(team, wire, from, tick)`.
pub(super) type Seen = Vec<(u64, u64, usize, u64)>;

/// A one-group logic with no world of its own: the TEAMS hook plays
/// back a script of exports (`None` once it runs out) and reports every
/// import view it is handed.
pub(super) struct TeamStub {
    pub(super) script: VecDeque<Option<TeamExport>>,
    pub(super) seen: mpsc::UnboundedSender<Seen>,
}

impl GameLogic<TWorld> for TeamStub {
    type GroupKey = ();
    type Strip = TStrip;

    fn snapshot_op(&self) -> u16 {
        0x7190
    }
    fn private_op(&self) -> u16 {
        0x7191
    }
    fn group_of(&self, _w: &TWorld, _p: PlayerId) -> Self::GroupKey {}
    fn snapshot(
        &mut self,
        _w: &mut TWorld,
        _ctx: &TickCtx,
        _g: &Self::GroupKey,
        _borrowed: &[BorderRecord<TStrip>],
        _out: &mut bytes::BytesMut,
    ) -> bool {
        false
    }
    fn on_join(&mut self, _w: &mut TWorld, conn: ConnectionId) -> Admission {
        Admission {
            player: PlayerId(conn.0),
            entity: conn.0,
        }
    }
    fn on_leave(&mut self, _w: &mut TWorld, _player: PlayerId) {}
    fn ingest(&mut self, _w: &mut TWorld, _c: &TickCtx, actions: &mut Vec<Action>) {
        actions.clear();
    }
    fn update(&mut self, _w: &mut TWorld, _c: &TickCtx) {}
}

impl ShardLogic<TWorld> for TeamStub {
    type State = TState;

    fn index(&self) -> usize {
        2
    }
    fn shard_count(&self) -> usize {
        4
    }
    fn serial_base(&self) -> u64 {
        2 * SHARD_SERIAL_RANGE
    }
    fn serial_range(&self) -> u64 {
        SHARD_SERIAL_RANGE
    }
    fn serial_used(&self) -> u64 {
        0
    }
    fn neighbors(&self) -> &[usize] {
        &[]
    }
    fn collect_migrations(&mut self, _w: &mut TWorld, _nb: usize) -> Vec<Migrating<TState>> {
        Vec::new()
    }
    fn on_migrate_in(&mut self, _w: &mut TWorld, _: u64, _: TState, _: Option<PlayerId>) {}
    fn on_migrate_out(&mut self, _w: &mut TWorld, _wire: u64) {}
    fn collect_border(&self, _w: &TWorld) -> Vec<BorderRecord<TStrip>> {
        Vec::new()
    }
    fn own_wires(&self, _w: &TWorld) -> Vec<u64> {
        Vec::new()
    }

    fn team_exchange(
        &mut self,
        _world: &mut TWorld,
        _ctx: &TickCtx,
        _borrowed: &[BorderRecord<TStrip>],
        imported: &TeamImports,
    ) -> Option<TeamExport> {
        let seen = imported
            .teams()
            .flat_map(|t| {
                imported
                    .team(t)
                    .iter()
                    .map(move |r| (t, r.wire, r.from, r.tick))
            })
            .collect();
        let _ = self.seen.send(seen);
        self.script.pop_front().flatten()
    }
}
