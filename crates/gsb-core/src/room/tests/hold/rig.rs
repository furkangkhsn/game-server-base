//! The veto-ceiling rig: a logic whose `may_release` answer the TEST
//! flips (and counts), and a synchronous room driven straight through
//! `handle_control` / `step_phases`.
//!
//! Wall-clock time without sleeping: the sweep compares the row's
//! absolute hold instants with `Instant::now()`, so [`Rig::age`] moves a
//! parked row's deadline and ceiling into the past by exactly the
//! simulated elapsed time — the same thing waiting would do, to the
//! nanosecond, and deterministic.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use super::*;

/// The test's handle on the logic's veto: `set(true)` = "in combat".
#[derive(Clone, Default)]
pub(super) struct Veto {
    on: Arc<AtomicBool>,
    asks: Arc<AtomicU32>,
}

impl Veto {
    pub(super) fn set(&self, on: bool) {
        self.on.store(on, Ordering::Relaxed);
    }
    /// How many times the core asked `may_release` so far.
    pub(super) fn asks(&self) -> u32 {
        self.asks.load(Ordering::Relaxed)
    }
}

struct VetoLogic {
    veto: Veto,
    decision: Detach,
    ended: mpsc::Sender<(PlayerId, ExpireTo)>,
}

impl GameLogic<()> for VetoLogic {
    type GroupKey = ();
    type Strip = ();
    fn snapshot_op(&self) -> u16 {
        0x7800
    }
    fn private_op(&self) -> u16 {
        0x7801
    }
    fn group_of(&self, _w: &(), _p: PlayerId) -> Self::GroupKey {}
    fn snapshot(
        &mut self,
        _w: &mut (),
        _c: &TickCtx,
        _g: &(),
        _borrowed: &[crate::shard::BorderRecord<()>],
        _o: &mut bytes::BytesMut,
    ) -> bool {
        false
    }
    fn on_join(&mut self, _w: &mut (), conn: ConnectionId) -> Admission {
        // Test identity policy: the conn id doubles as the player id.
        Admission {
            player: PlayerId(conn.0),
            entity: conn.0,
        }
    }
    fn on_leave(&mut self, _w: &mut (), _p: PlayerId) {}
    fn ingest(&mut self, _w: &mut (), _c: &TickCtx, a: &mut Vec<Action>) {
        a.clear();
    }
    fn update(&mut self, _w: &mut (), _c: &TickCtx) {}
    fn on_disconnect(&mut self, _w: &mut (), _p: PlayerId, _identity: &str) -> Detach {
        self.decision
    }
    fn may_release(&mut self, _w: &mut (), _p: PlayerId) -> bool {
        self.veto.asks.fetch_add(1, Ordering::Relaxed);
        !self.veto.on.load(Ordering::Relaxed)
    }
    fn on_detach_expired(&mut self, _w: &mut (), player: PlayerId, to: ExpireTo) {
        let _ = self.ended.try_send((player, to));
    }
}

impl RoomLogic<()> for VetoLogic {}

pub(super) struct Rig {
    pub(super) actor: RoomActor<(), (), ()>,
    pub(super) veto: Veto,
    ended: mpsc::Receiver<(PlayerId, ExpireTo)>,
    tick: u64,
    t0: Instant,
    _outs: Vec<mpsc::Receiver<FrameBatch>>,
}

impl Rig {
    /// A room whose policy answers every disconnect with `decision`,
    /// under `ceiling` ([`RoomConfig::max_detach_hold`]).
    pub(super) fn new(decision: Detach, ceiling: Option<Duration>) -> Self {
        let veto = Veto::default();
        let (ended_tx, ended) = mpsc::channel(64);
        let (_tick_tx, tick_rx) = broadcast::channel(4);
        let (_control, control_rx) = channel(16);
        let config = RoomConfig {
            id: RoomId(70),
            max_detach_hold: ceiling,
            ..Default::default()
        };
        let logic = VetoLogic {
            veto: veto.clone(),
            decision,
            ended: ended_tx,
        };
        let actor = RoomActor::new(
            config,
            (),
            Box::new(logic),
            tick_rx,
            control_rx,
            1,
            null_metrics_tx(),
            None,
        );
        Self {
            actor,
            veto,
            ended,
            tick: 0,
            t0: Instant::now(),
            _outs: Vec::new(),
        }
    }

    /// Join `conn`, then kill its transport: the policy parks it.
    pub(super) fn join_and_detach(&mut self, conn: u64) -> PlayerId {
        let conn = ConnectionId(conn);
        let (out, out_rx) = mpsc::channel::<FrameBatch>(8);
        self._outs.push(out_rx);
        let (reply, mut reply_rx) = oneshot::channel();
        self.actor
            .handle_control(RoomControl::Join { conn, out, reply });
        let (entity, _actions) = reply_rx.try_recv().expect("sync reply").expect("joined");
        self.actor.handle_control(RoomControl::Detach {
            conn,
            entity,
            identity: format!("id{}", conn.0),
        });
        let player = PlayerId(conn.0);
        assert!(self.actor.conns[&player].detached, "the policy parked it");
        player
    }

    /// Let `by` of wall-clock time pass for `player`'s hold.
    pub(super) fn age(&mut self, player: PlayerId, by: Duration) {
        let rc = self.actor.conns.get_mut(&player).expect("a held row");
        let back = |t: Instant| t.checked_sub(by).expect("representable");
        rc.detach_deadline = rc.detach_deadline.map(back);
        rc.detach_ceiling = rc.detach_ceiling.map(back);
    }

    /// One tick (its phase 0c is the sweep under test).
    pub(super) fn step(&mut self) {
        self.tick += 1;
        let at = self.t0 + Duration::from_millis(self.tick * 33);
        assert!(self.actor.step_phases(&TickInfo {
            tick: self.tick,
            at
        }));
    }

    /// Whether `player` is still held (parked, not ended).
    pub(super) fn held(&self, player: PlayerId) -> bool {
        self.actor
            .conns
            .get(&player)
            .is_some_and(|rc| rc.detached && !rc.bot_fed)
    }

    /// The holds that ended since the last call, as `on_detach_expired`
    /// saw them.
    pub(super) fn ended(&mut self) -> Vec<(PlayerId, ExpireTo)> {
        let mut v = Vec::new();
        while let Ok(x) = self.ended.try_recv() {
            v.push(x);
        }
        v
    }

    /// `(detach_expired_despawn, detach_expired_ai, detach_forced,
    /// ceiling warns)` — the forced count as the SAMPLE carries it.
    pub(super) fn counts(&self) -> (u64, u64, u64, u32) {
        let m = &self.actor.m;
        (
            m.detach_expired_despawn,
            m.detach_expired_ai,
            self.actor.sample().detach_forced,
            self.actor.detach_ceiling_warns,
        )
    }
}
