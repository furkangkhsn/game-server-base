//! The kick verb (BACKLOG E8) on a shard actor: the room actor's rules,
//! plus the one only a shard has — a kick never races its member's
//! migration. What the input and systems hooks ask is applied BEFORE
//! MIGRATE (the kicked member does not cross), what the broadcast-phase
//! hooks ask after it (a member that crossed is no longer this shard's).

use super::*;
use crate::conn::ServerClose;
use crate::registry::{CloseRequest, RegistryMsg};
use crate::room::{Detach, ExpireTo};

mod cases;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum At {
    Update,
    Snapshot,
}

/// [`TLogic`] (two shards, one seam at x = 0) that kicks from a plan:
/// `(tick, where, who, why)`.
struct Kicking {
    inner: TLogic,
    plan: Vec<(u64, At, PlayerId, String)>,
    disc: mpsc::Sender<(PlayerId, String)>,
    decision: Detach,
}

impl Kicking {
    fn ask(&self, ctx: &TickCtx, at: At) {
        for (tick, when, player, why) in &self.plan {
            if *tick == ctx.tick && *when == at {
                ctx.kick(*player, why.clone());
            }
        }
    }
}

impl GameLogic<TWorld> for Kicking {
    type GroupKey = ();
    type Strip = TStrip;
    fn snapshot_op(&self) -> u16 {
        self.inner.snapshot_op()
    }
    fn private_op(&self) -> u16 {
        self.inner.private_op()
    }
    fn group_of(&self, w: &TWorld, p: PlayerId) -> Self::GroupKey {
        self.inner.group_of(w, p)
    }
    fn snapshot(
        &mut self,
        w: &mut TWorld,
        ctx: &TickCtx,
        g: &(),
        borrowed: &[BorderRecord<TStrip>],
        out: &mut bytes::BytesMut,
    ) -> bool {
        self.ask(ctx, At::Snapshot);
        self.inner.snapshot(w, ctx, g, borrowed, out)
    }
    fn on_join(&mut self, w: &mut TWorld, conn: ConnectionId) -> Admission {
        self.inner.on_join(w, conn)
    }
    fn on_leave(&mut self, w: &mut TWorld, player: PlayerId) {
        self.inner.on_leave(w, player);
    }
    fn ingest(&mut self, w: &mut TWorld, ctx: &TickCtx, actions: &mut Vec<Action>) {
        self.inner.ingest(w, ctx, actions);
    }
    fn update(&mut self, w: &mut TWorld, ctx: &TickCtx) {
        self.inner.update(w, ctx);
        self.ask(ctx, At::Update);
    }
    fn on_disconnect(&mut self, _w: &mut TWorld, player: PlayerId, identity: &str) -> Detach {
        let _ = self.disc.try_send((player, identity.to_string()));
        self.decision
    }
}

impl ShardLogic<TWorld> for Kicking {
    type State = TState;
    fn index(&self) -> usize {
        self.inner.index()
    }
    fn shard_count(&self) -> usize {
        self.inner.shard_count()
    }
    fn serial_capacity(&self) -> u64 {
        self.inner.serial_capacity()
    }
    fn serial_used(&self) -> u64 {
        self.inner.serial_used()
    }
    fn neighbors(&self) -> &[usize] {
        self.inner.neighbors()
    }
    fn collect_migrations(&mut self, w: &mut TWorld, nb: usize) -> Vec<Migrating<TState>> {
        self.inner.collect_migrations(w, nb)
    }
    fn on_migrate_in(&mut self, w: &mut TWorld, wire: u64, s: TState, p: Option<PlayerId>) {
        self.inner.on_migrate_in(w, wire, s, p);
    }
    fn on_migrate_out(&mut self, w: &mut TWorld, wire: u64) {
        self.inner.on_migrate_out(w, wire);
    }
    fn collect_border(&self, w: &TWorld) -> Vec<BorderRecord<TStrip>> {
        self.inner.collect_border(w)
    }
    fn own_wires(&self, w: &TWorld) -> Vec<u64> {
        self.inner.own_wires(w)
    }
}

pub(super) struct Rig {
    a: ShardActor<TWorld, (), TState, TStrip>,
    /// Shard 1's inbox: what this shard (0) handed on.
    n1: mpsc::Receiver<ShardMsg<TState, TStrip>>,
    reg: mpsc::Receiver<RegistryMsg>,
    disc: mpsc::Receiver<(PlayerId, String)>,
    _outs: Vec<mpsc::Receiver<FrameBatch>>,
}

impl Rig {
    fn new(decision: Detach, plan: Vec<(u64, At, PlayerId, String)>) -> Self {
        let (n0, _n0_rx) = channel::<ShardMsg<TState, TStrip>>(8);
        let (n1, n1_rx) = channel::<ShardMsg<TState, TStrip>>(8);
        let (_tick_tx, tick_rx) = broadcast::channel(16);
        let (_self_tx, rx) = channel::<ShardMsg<TState, TStrip>>(16);
        let (disc_tx, disc) = mpsc::channel(64);
        let (reg_tx, reg) = channel::<RegistryMsg>(64);
        let logic = Kicking {
            inner: rig_logic(0),
            plan,
            disc: disc_tx,
            decision,
        };
        let cfg = RoomConfig {
            id: RoomId(70),
            keepalive_hz: 0.0,
            metrics_cadence_hz: 0.0,
            ..Default::default()
        };
        let a = ShardActor::new(
            cfg,
            0,
            TWorld::default(),
            Box::new(logic),
            tick_rx,
            rx,
            vec![n0, n1],
            1,
            metrics_null(),
            None,
        )
        .with_registry(reg_tx);
        Self {
            a,
            n1: n1_rx,
            reg,
            disc,
            _outs: Vec::new(),
        }
    }

    /// Join `conn` as `identity` (conn 1 spawns at x = -9, deep in shard
    /// 0; conn 10 at x = 0, shard 1's region: it crosses on its first
    /// tick).
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

    fn step(&mut self, tick: u64) {
        self.a.step_phases(&tinfo(tick));
    }

    fn disconnects(&mut self) -> Vec<(PlayerId, String)> {
        let mut v = Vec::new();
        while let Ok(x) = self.disc.try_recv() {
            v.push(x);
        }
        v
    }

    fn closes(&mut self) -> Vec<CloseRequest> {
        let mut v = Vec::new();
        while let Ok(m) = self.reg.try_recv() {
            if let RegistryMsg::CloseConn(r) = m {
                v.push(r);
            }
        }
        v
    }

    /// The players shard 0 handed on to shard 1, with their park flag.
    fn crossed(&mut self) -> Vec<(PlayerId, bool)> {
        let mut v = Vec::new();
        while let Ok(m) = self.n1.try_recv() {
            if let ShardMsg::Migrate {
                player: Some(p), ..
            } = m
            {
                v.push((p.player, p.detached));
            }
        }
        v
    }
}

pub(super) fn kick(tick: u64, at: At, player: u64, why: &str) -> (u64, At, PlayerId, String) {
    (tick, at, PlayerId(player), why.to_string())
}

/// The shard runs the room's path: the policy once, with the resume
/// identity; the close request (kicked, the game's reason) next tick.
#[tokio::test]
async fn a_shard_kick_runs_the_policy_then_asks_for_the_close() {
    let mut r = Rig::new(Detach::Despawn, vec![kick(2, At::Update, 1, "griefing")]);
    let entity = r.join(ConnectionId(1), "ana");
    r.step(2);
    assert_eq!(r.disconnects(), vec![(PlayerId(1), "ana".to_string())]);
    assert!(!r.a.conns.contains_key(&PlayerId(1)), "despawned");
    r.step(3);
    let got = r.closes();
    assert_eq!(got.len(), 1, "{got:?}");
    assert_eq!(
        (got[0].conn, got[0].entity, got[0].parked, got[0].cause),
        (ConnectionId(1), entity, false, ServerClose::Kicked)
    );
    assert_eq!(got[0].reason, "kicked: griefing");
}
