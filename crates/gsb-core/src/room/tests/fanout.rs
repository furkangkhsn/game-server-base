//! The broadcast phase's contracts: every dirty group emits on the
//! same tick, a full outbound channel drops and recovers, and an
//! unchanged group stays silent until its keep-alive.

use super::*;

mod keepalive;

mod fairness;

struct FairLogic {
    last_world: u64,
    last_emitted: HashMap<PlayerId, u64>,
    step_no: u64,
    steps: mpsc::Sender<u64>,
}

impl GameLogic<()> for FairLogic {
    type GroupKey = PlayerId;
    type Strip = ();

    fn snapshot_op(&self) -> u16 {
        0x7020
    }
    fn private_op(&self) -> u16 {
        0x7021
    }

    fn group_of(&self, _w: &(), player: PlayerId) -> Self::GroupKey {
        player
    }

    fn snapshot(
        &mut self,
        _w: &mut (),
        _c: &TickCtx,
        group: &Self::GroupKey,
        _borrowed: &[crate::shard::BorderRecord<()>],
        out: &mut bytes::BytesMut,
    ) -> bool {
        // "Unchanged" = the world step this group emitted at is the
        // current one. The map is keyed by `group` — per-group
        // bookkeeping, so one group's emission cannot make another
        // group's answer change in the same tick.
        if self.last_emitted.get(group).copied() == Some(self.last_world) {
            return false;
        }
        self.last_emitted.insert(*group, self.last_world);
        out.extend_from_slice(&self.last_world.to_le_bytes());
        true
    }

    fn on_join(&mut self, _w: &mut (), conn: ConnectionId) -> Admission {
        // Test identity policy: the conn id doubles as the player id.
        Admission {
            player: PlayerId(conn.0),
            entity: 1,
        }
    }
    fn on_leave(&mut self, _w: &mut (), _player: PlayerId) {}
    fn ingest(&mut self, _w: &mut (), _c: &TickCtx, actions: &mut Vec<Action>) {
        actions.clear();
    }
    fn update(&mut self, _w: &mut (), _c: &TickCtx) {
        // The world changes on every tick (e.g. one entity moving).
        self.last_world += 1;
        self.step_no += 1;
        let _ = self.steps.try_send(self.step_no);
    }
}

// Faz 1 trait split: shared contract on `GameLogic`; no room-exclusive
// hook used (empty `RoomLogic` impl).
impl RoomLogic<()> for FairLogic {}
