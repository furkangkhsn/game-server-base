//! The shard actor's three ends of a membership reach the disconnect
//! policy with their causes (BACKLOG F27) — the room actor's rule,
//! mirrored: `ShardMsg::Detach` → `ConnectionClosed`, the input-idle
//! ceiling under both `afk_action`s → `IdleInput`, the game's kick →
//! `Kicked`.

use super::*;
use crate::registry::RegistryMsg;
use crate::room::{AfkAction, Detach, DisconnectCause, ExpireTo};

/// One shard with no neighbours; logs every `(player, cause)` the policy
/// is asked, kicks per plan from `update`, and parks.
struct CauseLogic {
    next_wire: u64,
    seen: mpsc::Sender<(PlayerId, DisconnectCause)>,
    kick: Option<(u64, PlayerId)>,
}

impl GameLogic<TWorld> for CauseLogic {
    type GroupKey = ();
    type Strip = TStrip;
    fn snapshot_op(&self) -> u16 {
        0x7b10
    }
    fn private_op(&self) -> u16 {
        0x7b11
    }
    fn group_of(&self, _w: &TWorld, _p: PlayerId) -> Self::GroupKey {}
    fn snapshot(
        &mut self,
        _w: &mut TWorld,
        _c: &TickCtx,
        _g: &(),
        _b: &[BorderRecord<TStrip>],
        _o: &mut bytes::BytesMut,
    ) -> bool {
        false
    }
    fn on_join(&mut self, w: &mut TWorld, conn: ConnectionId) -> Admission {
        self.next_wire += 1;
        w.ents.insert(self.next_wire, (0.0, 0.0, 0));
        Admission {
            player: PlayerId(conn.0),
            entity: self.next_wire,
        }
    }
    fn on_leave(&mut self, _w: &mut TWorld, _p: PlayerId) {}
    fn on_disconnect_with(
        &mut self,
        _w: &mut TWorld,
        player: PlayerId,
        _identity: &str,
        cause: DisconnectCause,
    ) -> Detach {
        let _ = self.seen.try_send((player, cause));
        Detach::Hold {
            grace: Some(Duration::from_secs(60)),
            to: ExpireTo::Despawn,
        }
    }
    fn ingest(&mut self, _w: &mut TWorld, _c: &TickCtx, a: &mut Vec<Action>) {
        a.clear();
    }
    fn update(&mut self, _w: &mut TWorld, ctx: &TickCtx) {
        if let Some((tick, player)) = self.kick
            && tick == ctx.tick
        {
            ctx.kick(player, "cause test");
        }
    }
}

impl ShardLogic<TWorld> for CauseLogic {
    type State = TState;
    fn index(&self) -> usize {
        0
    }
    fn shard_count(&self) -> usize {
        1
    }
    fn serial_capacity(&self) -> u64 {
        SHARD_SERIAL_CAPACITY
    }
    fn serial_used(&self) -> u64 {
        self.next_wire
    }
    fn neighbors(&self) -> &[usize] {
        &[]
    }
    fn collect_migrations(&mut self, _w: &mut TWorld, _nb: usize) -> Vec<Migrating<TState>> {
        Vec::new()
    }
    fn on_migrate_in(&mut self, _w: &mut TWorld, _wire: u64, _s: TState, _p: Option<PlayerId>) {}
    fn on_migrate_out(&mut self, _w: &mut TWorld, _wire: u64) {}
    fn collect_border(&self, _w: &TWorld) -> Vec<BorderRecord<TStrip>> {
        Vec::new()
    }
    fn own_wires(&self, w: &TWorld) -> Vec<u64> {
        w.ents.keys().copied().collect()
    }
}

struct Rig {
    a: ShardActor<TWorld, (), TState, TStrip>,
    seen: mpsc::Receiver<(PlayerId, DisconnectCause)>,
    t0: Instant,
    _reg: mpsc::Receiver<RegistryMsg>,
    _outs: Vec<mpsc::Receiver<FrameBatch>>,
}

impl Rig {
    fn new(ceiling: Option<u64>, afk_action: AfkAction, kick: Option<(u64, PlayerId)>) -> Self {
        let (seen_tx, seen) = mpsc::channel(64);
        let (_tick_tx, tick_rx) = broadcast::channel(16);
        let (_self_tx, rx) = channel::<ShardMsg<TState, TStrip>>(16);
        let (reg_tx, reg) = channel::<RegistryMsg>(64);
        let cfg = RoomConfig {
            id: RoomId(91),
            keepalive_hz: 0.0,
            metrics_cadence_hz: 0.0,
            max_idle_input_secs: ceiling,
            afk_action,
            ..Default::default()
        };
        let logic = CauseLogic {
            next_wire: 0,
            seen: seen_tx,
            kick,
        };
        let a = ShardActor::new(
            cfg,
            0,
            TWorld::default(),
            Box::new(logic),
            tick_rx,
            rx,
            vec![],
            1,
            metrics_null(),
            None,
        )
        .with_registry(reg_tx);
        Self {
            a,
            seen,
            t0: Instant::now(),
            _reg: reg,
            _outs: Vec::new(),
        }
    }

    fn join(&mut self, conn: ConnectionId, identity: &str) -> EntityId {
        let (out, out_rx) = mpsc::channel::<FrameBatch>(64);
        self._outs.push(out_rx);
        let (reply, mut reply_rx) = oneshot::channel();
        assert!(self.a.handle_msg(
            ShardMsg::Join {
                conn,
                epoch: 1,
                identity: identity.to_string(),
                out,
                reply,
                claims: None,
            },
            1,
        ));
        reply_rx.try_recv().expect("synchronous").expect("joined").0
    }

    fn detach(&mut self, conn: ConnectionId, entity: EntityId, identity: &str) {
        assert!(self.a.handle_msg(
            ShardMsg::Detach {
                conn,
                entity,
                identity: identity.to_string(),
            },
            1,
        ));
    }

    fn step_at(&mut self, tick: u64, secs: u64) {
        self.a.step_phases(&TickInfo {
            tick,
            at: self.t0 + Duration::from_secs(secs),
        });
    }

    fn seen(&mut self) -> Vec<(PlayerId, DisconnectCause)> {
        std::iter::from_fn(|| self.seen.try_recv().ok()).collect()
    }
}

#[tokio::test]
async fn a_closed_connection_reaches_the_shard_policy_as_connection_closed() {
    let mut r = Rig::new(None, AfkAction::LeaveRoom, None);
    let entity = r.join(ConnectionId(1), "ana");
    r.detach(ConnectionId(1), entity, "ana");
    assert_eq!(
        r.seen(),
        vec![(PlayerId(1), DisconnectCause::ConnectionClosed)]
    );
}

#[tokio::test]
async fn the_shard_idle_ceiling_reaches_the_policy_as_idle_input_under_both_actions() {
    for action in [AfkAction::LeaveRoom, AfkAction::Disconnect] {
        let mut r = Rig::new(Some(5), action, None);
        let entity = r.join(ConnectionId(1), "ana");
        r.step_at(1, 1);
        r.step_at(2, 6);
        assert_eq!(
            r.seen(),
            vec![(PlayerId(1), DisconnectCause::IdleInput)],
            "{action:?}"
        );
        r.detach(ConnectionId(1), entity, "ana");
        assert!(r.seen().is_empty(), "{action:?}: asked once");
    }
}

#[tokio::test]
async fn a_shard_kick_reaches_the_policy_as_kicked() {
    let mut r = Rig::new(None, AfkAction::LeaveRoom, Some((1, PlayerId(2))));
    r.join(ConnectionId(1), "ana");
    r.join(ConnectionId(2), "bora");
    r.step_at(1, 1);
    assert_eq!(r.seen(), vec![(PlayerId(2), DisconnectCause::Kicked)]);
}
