//! The shard veto-ceiling rig: the room rig's vetoing logic as a
//! [`ShardLogic`] (two regions; a test-chosen player can be pushed
//! across the seam), and a synchronous shard driven through
//! `handle_msg` / `step_phases`. Time is simulated the same way: `age`
//! moves a held row's absolute instants into the past.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use super::*;
use crate::room::Detach;

type Actor = ShardActor<(), (), (), ()>;
pub(super) type Msg = ShardMsg<(), ()>;

/// The test's handle on every shard's logic: the veto, how often it was
/// asked, and the player (`0` = none) to push across the seam.
#[derive(Clone, Default)]
pub(super) struct Knobs {
    veto: Arc<AtomicBool>,
    asks: Arc<AtomicU32>,
    evict: Arc<AtomicU64>,
}

impl Knobs {
    pub(super) fn veto(&self, on: bool) {
        self.veto.store(on, Ordering::Relaxed);
    }
    pub(super) fn asks(&self) -> u32 {
        self.asks.load(Ordering::Relaxed)
    }
    pub(super) fn evict(&self, player: PlayerId) {
        self.evict.store(player.0, Ordering::Relaxed);
    }
}

struct VetoShard {
    index: usize,
    others: [usize; 1],
    knobs: Knobs,
    decision: Detach,
    wires: HashMap<PlayerId, u64>,
}

impl GameLogic<()> for VetoShard {
    type GroupKey = ();
    type Strip = ();
    fn snapshot_op(&self) -> u16 {
        0x7900
    }
    fn private_op(&self) -> u16 {
        0x7901
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
    fn on_join(&mut self, _w: &mut (), conn: ConnectionId) -> Admission {
        let player = PlayerId(conn.0);
        let wire = self.index as u64 * SHARD_SERIAL_RANGE + conn.0;
        self.wires.insert(player, wire);
        Admission {
            player,
            entity: wire,
        }
    }
    fn on_leave(&mut self, _w: &mut (), player: PlayerId) {
        self.wires.remove(&player);
    }
    fn ingest(&mut self, _w: &mut (), _c: &TickCtx, a: &mut Vec<Action>) {
        a.clear();
    }
    fn update(&mut self, _w: &mut (), _c: &TickCtx) {}
    fn on_disconnect(&mut self, _w: &mut (), _p: PlayerId, _identity: &str) -> Detach {
        self.decision
    }
    fn may_release(&mut self, _w: &mut (), _p: PlayerId) -> bool {
        self.knobs.asks.fetch_add(1, Ordering::Relaxed);
        !self.knobs.veto.load(Ordering::Relaxed)
    }
}

impl ShardLogic<()> for VetoShard {
    type State = ();
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
        self.wires.len() as u64
    }
    fn neighbors(&self) -> &[usize] {
        &self.others
    }
    fn collect_migrations(&mut self, _w: &mut (), _nb: usize) -> Vec<Migrating<()>> {
        let player = PlayerId(self.knobs.evict.swap(0, Ordering::Relaxed));
        match self.wires.remove(&player) {
            Some(wire) => vec![Migrating {
                wire,
                state: (),
                player: Some(player),
            }],
            None => Vec::new(),
        }
    }
    fn on_migrate_in(&mut self, _w: &mut (), wire: u64, _s: (), player: Option<PlayerId>) {
        if let Some(p) = player {
            self.wires.insert(p, wire);
        }
    }
    fn on_migrate_out(&mut self, _w: &mut (), _wire: u64) {}
    fn collect_border(&self, _w: &()) -> Vec<BorderRecord<()>> {
        Vec::new()
    }
    fn own_wires(&self, _w: &()) -> Vec<u64> {
        self.wires.values().copied().collect()
    }
}

/// Shard `index` of a two-shard room under `ceiling`; `to_other` is the
/// mailbox its crossings go to.
pub(super) fn shard(
    index: usize,
    knobs: &Knobs,
    decision: Detach,
    ceiling: Option<Duration>,
    to_other: Mailbox<Msg>,
) -> Actor {
    let (_tick_tx, tick_rx) = broadcast::channel(4);
    let (_self_tx, rx) = channel::<Msg>(16);
    let (dummy, _d) = channel::<Msg>(1);
    let links = if index == 0 {
        vec![dummy, to_other]
    } else {
        vec![to_other, dummy]
    };
    let logic = VetoShard {
        index,
        others: [1 - index],
        knobs: knobs.clone(),
        decision,
        wires: HashMap::new(),
    };
    let config = RoomConfig {
        id: RoomId(71),
        keepalive_hz: 0.0,
        metrics_cadence_hz: 0.0,
        max_detach_hold: ceiling,
        ..Default::default()
    };
    ShardActor::new(
        config,
        index,
        (),
        Box::new(logic),
        tick_rx,
        rx,
        links,
        1,
        metrics_null(),
        None,
    )
}

/// Join `conn` on `a`, then kill its transport: the policy parks it.
pub(super) fn join_and_detach(a: &mut Actor, conn: u64) -> PlayerId {
    let conn = ConnectionId(conn);
    let (out, _out_rx) = mpsc::channel::<FrameBatch>(8);
    let (reply, mut reply_rx) = oneshot::channel();
    let identity = format!("id{}", conn.0);
    let join = ShardMsg::Join {
        conn,
        epoch: 1,
        identity: identity.clone(),
        out,
        reply,
    };
    assert!(a.handle_msg(join, 1));
    let (entity, _actions) = reply_rx.try_recv().expect("sync reply").expect("joined");
    let detach = ShardMsg::Detach {
        conn,
        entity,
        identity,
    };
    assert!(a.handle_msg(detach, 1));
    let player = PlayerId(conn.0);
    assert!(a.conns[&player].detached, "the policy parked it");
    player
}

/// Let `by` of wall-clock time pass for `player`'s hold on `a`.
pub(super) fn age(a: &mut Actor, player: PlayerId, by: Duration) {
    let rc = a.conns.get_mut(&player).expect("a held row");
    let back = |t: Instant| t.checked_sub(by).expect("representable");
    rc.detach_deadline = rc.detach_deadline.map(back);
    rc.detach_ceiling = rc.detach_ceiling.map(back);
}

/// One tick at global tick `tick` (phase 0c is the sweep under test).
pub(super) fn step(a: &mut Actor, tick: u64) {
    assert!(a.step_phases(&tinfo(tick)));
}

/// Whether `player` is still held on `a`.
pub(super) fn held(a: &Actor, player: PlayerId) -> bool {
    a.conns
        .get(&player)
        .is_some_and(|rc| rc.detached && !rc.bot_fed)
}

/// `(detach_expired_despawn, detach_expired_ai, ceiling warns)` of `a`.
pub(super) fn counts(a: &Actor) -> (u64, u64, u32) {
    (
        a.m.detach_expired_despawn,
        a.m.detach_expired_ai,
        a.detach_ceiling_warns,
    )
}
