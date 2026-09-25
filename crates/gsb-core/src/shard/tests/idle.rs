//! The shard's half of the AFK seam: the same input-idle signal and the
//! same (default-off) ceiling the single room carries, driven through a
//! real [`ShardActor`].

use super::*;
use crate::room::{Detach, ExpireTo};

struct IdleLogic {
    players: Vec<PlayerId>,
    next_wire: u64,
    obs: mpsc::Sender<(PlayerId, Option<Duration>)>,
    disc: mpsc::Sender<(PlayerId, String)>,
    decision: Detach,
}

impl GameLogic<TWorld> for IdleLogic {
    type GroupKey = ();
    type Strip = TStrip;

    fn snapshot_op(&self) -> u16 {
        0x7600
    }
    fn private_op(&self) -> u16 {
        0x7601
    }
    fn group_of(&self, _w: &TWorld, _p: PlayerId) -> Self::GroupKey {}
    fn snapshot(
        &mut self,
        _w: &mut TWorld,
        _c: &TickCtx,
        _g: &Self::GroupKey,
        _borrowed: &[BorderRecord<TStrip>],
        _o: &mut bytes::BytesMut,
    ) -> bool {
        false
    }
    fn on_join(&mut self, w: &mut TWorld, conn: ConnectionId) -> Admission {
        self.next_wire += 1;
        w.ents.insert(self.next_wire, (0.0, 0.0, 0));
        let player = PlayerId(conn.0);
        self.players.push(player);
        Admission {
            player,
            entity: self.next_wire,
        }
    }
    fn on_leave(&mut self, _w: &mut TWorld, player: PlayerId) {
        self.players.retain(|p| *p != player);
    }
    fn ingest(&mut self, _w: &mut TWorld, _c: &TickCtx, a: &mut Vec<Action>) {
        a.clear();
    }
    fn update(&mut self, _w: &mut TWorld, ctx: &TickCtx) {
        for p in &self.players {
            let _ = self.obs.try_send((*p, ctx.since_input(*p)));
        }
    }
    fn on_disconnect(&mut self, _w: &mut TWorld, player: PlayerId, identity: &str) -> Detach {
        let _ = self.disc.try_send((player, identity.to_string()));
        self.decision
    }
}

impl ShardLogic<TWorld> for IdleLogic {
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
    fn on_migrate_in(
        &mut self,
        _w: &mut TWorld,
        _wire: u64,
        _state: TState,
        _player: Option<PlayerId>,
    ) {
    }
    fn on_migrate_out(&mut self, _w: &mut TWorld, _wire: u64) {}
    fn collect_border(&self, _w: &TWorld) -> Vec<BorderRecord<TStrip>> {
        Vec::new()
    }
    fn own_wires(&self, w: &TWorld) -> Vec<u64> {
        w.ents.keys().copied().collect()
    }
}

#[allow(clippy::type_complexity)]
fn rig(
    ceiling: Option<u64>,
    decision: Detach,
) -> (
    ShardActor<TWorld, (), TState, TStrip>,
    mpsc::Receiver<(PlayerId, Option<Duration>)>,
    mpsc::Receiver<(PlayerId, String)>,
) {
    let (obs_tx, obs) = mpsc::channel(4096);
    let (disc_tx, disc) = mpsc::channel(64);
    let (_tick_tx, tick_rx) = broadcast::channel(16);
    let (_self_tx, rx) = channel::<ShardMsg<TState, TStrip>>(16);
    let a = ShardActor::new(
        RoomConfig {
            id: RoomId(61),
            tick_hz: 30.0,
            metrics_cadence_hz: 0.0,
            max_idle_input_secs: ceiling,
            ..Default::default()
        },
        0,
        TWorld::default(),
        Box::new(IdleLogic {
            players: Vec::new(),
            next_wire: 0,
            obs: obs_tx,
            disc: disc_tx,
            decision,
        }),
        tick_rx,
        rx,
        vec![],
        1,
        metrics_null(),
        None,
    );
    (a, obs, disc)
}

fn join(
    a: &mut ShardActor<TWorld, (), TState, TStrip>,
    conn: ConnectionId,
    identity: &str,
) -> mpsc::Receiver<FrameBatch> {
    let (out_tx, out_rx) = mpsc::channel::<FrameBatch>(64);
    let (reply_tx, mut reply_rx) = oneshot::channel();
    assert!(a.handle_msg(
        ShardMsg::Join {
            conn,
            epoch: 1,
            identity: identity.to_string(),
            out: out_tx,
            reply: reply_tx,
        },
        1,
    ));
    reply_rx
        .try_recv()
        .expect("join reply is synchronous")
        .expect("join accepted");
    out_rx
}

fn drain<T>(rx: &mut mpsc::Receiver<T>) -> Vec<T> {
    let mut v = Vec::new();
    while let Ok(x) = rx.try_recv() {
        v.push(x);
    }
    v
}

/// The shard carries the SAME signal: a member that never sends an
/// action is input-idle while staying a live, undetached member — and
/// with the ceiling unset (the default) the shard never acts on it.
#[tokio::test]
async fn sharded_idle_signal_reads_without_the_ceiling_ever_acting() {
    let (mut a, mut obs, mut disc) = rig(None, Detach::Despawn);
    let _out = join(&mut a, ConnectionId(1), "ana");
    let p = PlayerId(1);
    let t0 = Instant::now();
    for k in 1..=30u64 {
        a.step_phases(&TickInfo {
            tick: k,
            at: t0 + Duration::from_secs(k * 10),
        });
    }
    let last = drain(&mut obs)
        .into_iter()
        .rfind(|(q, _)| *q == p)
        .expect("observed");
    assert!(
        last.1.expect("on the clock") >= Duration::from_secs(290),
        "the shard's idle signal grows with the silence: {:?}",
        last.1
    );
    assert!(
        drain(&mut disc).is_empty(),
        "the DEFAULT config must never act on an idle member"
    );
    assert!(
        a.conns.contains_key(&p) && !a.conns[&p].detached,
        "the member is untouched"
    );
}

/// With the ceiling set, the shard hands the expired member to the same
/// `on_disconnect` policy a dead transport reaches — with the resume key
/// its row remembers — and warns once.
#[tokio::test]
async fn sharded_ceiling_runs_the_disconnect_policy_and_warns_once() {
    let (mut a, _obs, mut disc) = rig(
        Some(5),
        Detach::Hold {
            grace: Some(Duration::from_secs(60)),
            to: ExpireTo::AiHandover,
        },
    );
    let _o1 = join(&mut a, ConnectionId(1), "ana");
    let _o2 = join(&mut a, ConnectionId(2), "bora");
    let t0 = Instant::now();
    // Below the ceiling: nothing.
    a.step_phases(&TickInfo {
        tick: 1,
        at: t0 + Duration::from_secs(2),
    });
    assert!(drain(&mut disc).is_empty(), "below the ceiling, nothing");
    // Past it: both members reach the policy, which parks them.
    for k in 2..=6u64 {
        a.step_phases(&TickInfo {
            tick: k,
            at: t0 + Duration::from_secs(10 + k),
        });
    }
    let mut got = drain(&mut disc);
    got.sort_by_key(|(p, _)| p.0);
    assert_eq!(
        got,
        vec![
            (PlayerId(1), "ana".to_string()),
            (PlayerId(2), "bora".to_string())
        ],
        "each idle member reached on_disconnect once, with its own key"
    );
    assert!(
        a.conns[&PlayerId(1)].detached && a.conns[&PlayerId(2)].detached,
        "the POLICY parked them; the shard despawned nothing itself"
    );
    assert_eq!(
        a.idle_ceiling_warns, 1,
        "one warning for the shard, not one per member or per tick"
    );
    // Parked rows are off the clock: no re-fire, however long the hold.
    for k in 7..=40u64 {
        a.step_phases(&TickInfo {
            tick: k,
            at: t0 + Duration::from_secs(100 + k),
        });
    }
    assert!(
        drain(&mut disc).is_empty(),
        "a parked member is never double-counted by the idle ceiling"
    );
}
