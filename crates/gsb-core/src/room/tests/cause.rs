//! Each end of a membership reaches the disconnect policy with ITS cause
//! (BACKLOG F27): the room actor's three call sites — a closed
//! connection, the input-idle ceiling under both `afk_action`s, the
//! game's kick — each pass what they are, through
//! `GameLogic::on_disconnect_with`.

use super::*;
use crate::registry::RegistryMsg;
use crate::room::DisconnectCause;

/// Logs every `(player, cause)` the policy is asked, kicks per plan
/// from `update`, and answers `decision`.
struct CauseLogic {
    seen: mpsc::Sender<(PlayerId, DisconnectCause)>,
    kick: Option<(u64, PlayerId)>,
    decision: Detach,
}

impl GameLogic<()> for CauseLogic {
    type GroupKey = ();
    type Strip = ();
    fn snapshot_op(&self) -> u16 {
        0x7b00
    }
    fn private_op(&self) -> u16 {
        0x7b01
    }
    fn group_of(&self, _w: &(), _p: PlayerId) -> Self::GroupKey {}
    fn snapshot(
        &mut self,
        _w: &mut (),
        _c: &TickCtx,
        _g: &(),
        _b: &[crate::shard::BorderRecord<()>],
        _o: &mut bytes::BytesMut,
    ) -> bool {
        false
    }
    fn on_join(&mut self, _w: &mut (), conn: ConnectionId) -> Admission {
        Admission {
            player: PlayerId(conn.0),
            entity: 100 + conn.0,
        }
    }
    fn on_leave(&mut self, _w: &mut (), _p: PlayerId) {}
    fn on_disconnect_with(
        &mut self,
        _w: &mut (),
        player: PlayerId,
        _identity: &str,
        cause: DisconnectCause,
    ) -> Detach {
        let _ = self.seen.try_send((player, cause));
        self.decision
    }
    fn ingest(&mut self, _w: &mut (), _c: &TickCtx, a: &mut Vec<Action>) {
        a.clear();
    }
    fn update(&mut self, _w: &mut (), ctx: &TickCtx) {
        if let Some((tick, player)) = self.kick
            && tick == ctx.tick
        {
            ctx.kick(player, "cause test");
        }
    }
}

impl RoomLogic<()> for CauseLogic {}

struct Rig {
    actor: RoomActor<(), (), ()>,
    seen: mpsc::Receiver<(PlayerId, DisconnectCause)>,
    t0: Instant,
    _reg: mpsc::Receiver<RegistryMsg>,
    _outs: Vec<mpsc::Receiver<FrameBatch>>,
}

impl Rig {
    /// A room with a registry mailbox (so every action's registry half
    /// runs too) whose policy parks.
    fn new(cfg: RoomConfig, kick: Option<(u64, PlayerId)>) -> Self {
        let (seen_tx, seen) = mpsc::channel(64);
        let (_tick_tx, tick_rx) = broadcast::channel(4);
        let (_control, control_rx) = channel(16);
        let mut actor = RoomActor::new(
            cfg,
            (),
            Box::new(CauseLogic {
                seen: seen_tx,
                kick,
                decision: Detach::Hold {
                    grace: Some(Duration::from_secs(60)),
                    to: ExpireTo::Despawn,
                },
            }),
            tick_rx,
            control_rx,
            1,
            null_metrics_tx(),
            None,
        );
        let (reg_tx, reg) = channel(64);
        actor.registry = Some(reg_tx);
        Self {
            actor,
            seen,
            t0: Instant::now(),
            _reg: reg,
            _outs: Vec::new(),
        }
    }

    fn join(&mut self, conn: ConnectionId, identity: &str) -> EntityId {
        let (out_tx, out_rx) = mpsc::channel::<FrameBatch>(64);
        self._outs.push(out_rx);
        let (rtx, mut rrx) = oneshot::channel();
        self.actor.handle_control(RoomControl::Resume {
            conn,
            epoch: 1,
            identity: identity.to_string(),
            out: out_tx,
            reply: rtx,
            claims: None,
        });
        rrx.try_recv().expect("synchronous").expect("joined").0
    }

    fn step_at(&mut self, tick: u64, secs: u64) {
        self.actor.step_phases(&TickInfo {
            tick,
            at: self.t0 + Duration::from_secs(secs),
        });
    }

    fn seen(&mut self) -> Vec<(PlayerId, DisconnectCause)> {
        std::iter::from_fn(|| self.seen.try_recv().ok()).collect()
    }
}

fn cfg(ceiling: Option<u64>, afk_action: AfkAction) -> RoomConfig {
    RoomConfig {
        id: RoomId(90),
        keepalive_hz: 0.0,
        max_idle_input_secs: ceiling,
        afk_action,
        ..Default::default()
    }
}

/// The registry's transport-death route: `ConnectionClosed`.
#[test]
fn a_closed_connection_reaches_the_policy_as_connection_closed() {
    let mut r = Rig::new(cfg(None, AfkAction::LeaveRoom), None);
    let entity = r.join(ConnectionId(1), "ana");
    r.actor.handle_control(RoomControl::Detach {
        conn: ConnectionId(1),
        entity,
        identity: "ana".into(),
    });
    assert_eq!(
        r.seen(),
        vec![(PlayerId(1), DisconnectCause::ConnectionClosed)]
    );
}

/// The ceiling: `IdleInput` under both actions (the action decides the
/// connection's fate, not the cause) — and the connection's own close
/// that follows asks nothing more (the park already answered).
#[test]
fn the_idle_ceiling_reaches_the_policy_as_idle_input_under_both_actions() {
    for action in [AfkAction::LeaveRoom, AfkAction::Disconnect] {
        let mut r = Rig::new(cfg(Some(5), action), None);
        let entity = r.join(ConnectionId(1), "ana");
        r.step_at(1, 1);
        r.step_at(2, 6);
        assert_eq!(
            r.seen(),
            vec![(PlayerId(1), DisconnectCause::IdleInput)],
            "{action:?}"
        );
        r.actor.handle_control(RoomControl::Detach {
            conn: ConnectionId(1),
            entity,
            identity: "ana".into(),
        });
        assert!(r.seen().is_empty(), "{action:?}: asked once");
    }
}

/// The game's kick: `Kicked`.
#[test]
fn a_kick_reaches_the_policy_as_kicked() {
    let mut r = Rig::new(cfg(None, AfkAction::LeaveRoom), Some((1, PlayerId(2))));
    r.join(ConnectionId(1), "ana");
    r.join(ConnectionId(2), "bora");
    r.step_at(1, 1);
    assert_eq!(r.seen(), vec![(PlayerId(2), DisconnectCause::Kicked)]);
}
