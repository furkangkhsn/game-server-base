//! The game's kick verb (BACKLOG E8) on the room actor: a hook asks
//! through `ctx.kick`, the room runs the ordinary disconnect policy once
//! the hooks that could ask have returned, and asks the registry to close
//! the connection with the kicked verdict and the game's reason.

use super::*;
use crate::conn::ServerClose;
use crate::registry::{CloseRequest, RegistryMsg};

mod cases;

/// Where in the tick the test logic asks for a kick.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum At {
    Ingest,
    Update,
    Snapshot,
}

/// What the test logic saw, in order.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Ev {
    Ingest(u64),
    Update(u64),
    Snapshot(u64),
    Disconnect(PlayerId, String),
    Leave(PlayerId),
}

/// A kick the test logic asks for: `(tick, where, who, why)`.
type Plan = (u64, At, PlayerId, String);

struct KickLogic {
    players: Vec<PlayerId>,
    plan: Vec<Plan>,
    log: mpsc::Sender<Ev>,
    decision: Detach,
}

impl KickLogic {
    fn ask(&self, ctx: &TickCtx, at: At) {
        for (tick, when, player, why) in &self.plan {
            if *tick == ctx.tick && *when == at {
                ctx.kick(*player, why.clone());
            }
        }
    }
}

impl GameLogic<()> for KickLogic {
    type GroupKey = ();
    type Strip = ();
    fn snapshot_op(&self) -> u16 {
        0x7700
    }
    fn private_op(&self) -> u16 {
        0x7701
    }
    fn group_of(&self, _w: &(), _p: PlayerId) -> Self::GroupKey {}
    fn snapshot(
        &mut self,
        _w: &mut (),
        ctx: &TickCtx,
        _g: &(),
        _borrowed: &[crate::shard::BorderRecord<()>],
        out: &mut bytes::BytesMut,
    ) -> bool {
        let _ = self.log.try_send(Ev::Snapshot(ctx.tick));
        self.ask(ctx, At::Snapshot);
        out.extend_from_slice(&ctx.tick.to_le_bytes());
        true
    }
    fn on_join(&mut self, _w: &mut (), conn: ConnectionId) -> Admission {
        // Test identity policy: the conn id doubles as the player id.
        let player = PlayerId(conn.0);
        self.players.push(player);
        Admission {
            player,
            entity: 100 + conn.0,
        }
    }
    fn on_leave(&mut self, _w: &mut (), player: PlayerId) {
        let _ = self.log.try_send(Ev::Leave(player));
        self.players.retain(|p| *p != player);
    }
    fn ingest(&mut self, _w: &mut (), ctx: &TickCtx, a: &mut Vec<Action>) {
        a.clear();
        let _ = self.log.try_send(Ev::Ingest(ctx.tick));
        self.ask(ctx, At::Ingest);
    }
    fn update(&mut self, _w: &mut (), ctx: &TickCtx) {
        let _ = self.log.try_send(Ev::Update(ctx.tick));
        self.ask(ctx, At::Update);
    }
    fn on_disconnect(&mut self, _w: &mut (), player: PlayerId, identity: &str) -> Detach {
        let _ = self
            .log
            .try_send(Ev::Disconnect(player, identity.to_string()));
        self.decision
    }
}

impl RoomLogic<()> for KickLogic {}

struct Rig {
    actor: RoomActor<(), (), ()>,
    log: mpsc::Receiver<Ev>,
    outs: HashMap<ConnectionId, mpsc::Receiver<FrameBatch>>,
}

impl Rig {
    fn new(id: u64, decision: Detach, plan: Vec<Plan>) -> Self {
        let (log_tx, log) = mpsc::channel(4096);
        let (_tick_tx, tick_rx) = broadcast::channel(4);
        let (_control, control_rx) = channel(16);
        let cfg = RoomConfig {
            id: RoomId(id),
            keepalive_hz: 0.0,
            ..Default::default()
        };
        let logic = KickLogic {
            players: Vec::new(),
            plan,
            log: log_tx,
            decision,
        };
        let actor = RoomActor::new(
            cfg,
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
            log,
            outs: HashMap::new(),
        }
    }

    /// Give the room a registry mailbox of `cap` slots.
    fn registry(&mut self, cap: usize) -> mpsc::Receiver<RegistryMsg> {
        let (tx, rx) = channel(cap);
        self.actor.registry = Some(tx);
        rx
    }

    /// Join through the resume arm (an IDENTIFIED client's path), so the
    /// row remembers its resume key.
    fn join(&mut self, conn: ConnectionId, identity: &str) -> (EntityId, Mailbox<Action>) {
        let (out_tx, out_rx) = mpsc::channel::<FrameBatch>(64);
        self.outs.insert(conn, out_rx);
        let (rtx, mut rrx) = oneshot::channel();
        self.actor.handle_control(RoomControl::Resume {
            conn,
            epoch: 1,
            identity: identity.to_string(),
            out: out_tx,
            reply: rtx,
            claims: None,
        });
        rrx.try_recv().expect("synchronous").expect("join accepted")
    }

    fn step(&mut self, tick: u64) {
        self.actor.step_phases(&TickInfo {
            tick,
            at: Instant::now(),
        });
    }

    fn events(&mut self) -> Vec<Ev> {
        let mut v = Vec::new();
        while let Ok(e) = self.log.try_recv() {
            v.push(e);
        }
        v
    }

    fn disconnects(&mut self) -> usize {
        self.events()
            .iter()
            .filter(|e| matches!(e, Ev::Disconnect(..)))
            .count()
    }

    /// Batches `conn`'s connection received so far.
    fn batches(&mut self, conn: ConnectionId) -> usize {
        let rx = self.outs.get_mut(&conn).expect("joined");
        let mut n = 0;
        while rx.try_recv().is_ok() {
            n += 1;
        }
        n
    }
}

/// The close requests the registry received so far.
fn closes(rx: &mut mpsc::Receiver<RegistryMsg>) -> Vec<CloseRequest> {
    let mut v = Vec::new();
    while let Ok(msg) = rx.try_recv() {
        if let RegistryMsg::CloseConn(req) = msg {
            v.push(req);
        }
    }
    v
}

fn kick(tick: u64, at: At, player: u64, why: &str) -> Plan {
    (tick, at, PlayerId(player), why.to_string())
}

/// THE PATH: a kick asked in `ingest` runs `on_disconnect` once, with the
/// player's resume identity, right after SYSTEMS — before the broadcast,
/// so the kicked member gets no batch of that tick — and the despawn
/// policy removes it; the close request (kicked, the game's reason,
/// despawned) leaves at the next tick's phase 0d.
#[test]
fn a_kick_runs_the_disconnect_policy_then_asks_for_the_close() {
    let mut r = Rig::new(
        80,
        Detach::Despawn,
        vec![kick(1, At::Ingest, 1, "speed hack")],
    );
    let mut reg = r.registry(64);
    let (entity, _a1) = r.join(ConnectionId(1), "ana");
    let (_e2, _a2) = r.join(ConnectionId(2), "bora");
    r.step(1);
    assert_eq!(
        r.events(),
        vec![
            Ev::Ingest(1),
            Ev::Update(1),
            Ev::Disconnect(PlayerId(1), "ana".into()),
            Ev::Leave(PlayerId(1)),
            Ev::Snapshot(1),
        ],
        "after SYSTEMS, before BROADCAST; never inside the asking hook"
    );
    assert!(!r.actor.conns.contains_key(&PlayerId(1)), "despawned");
    assert_eq!(r.batches(ConnectionId(1)), 0, "no batch after the kick");
    assert_eq!(r.batches(ConnectionId(2)), 1, "the others play on");
    assert!(closes(&mut reg).is_empty(), "queued, not yet sent");
    r.step(2);
    let got = closes(&mut reg);
    assert_eq!(got.len(), 1, "{got:?}");
    let req = &got[0];
    assert_eq!(
        (req.conn, req.room, req.entity, req.parked, req.cause),
        (
            ConnectionId(1),
            RoomId(80),
            entity,
            false,
            ServerClose::Kicked
        )
    );
    assert_eq!(req.reason, "kicked: speed hack");
    r.step(3);
    assert!(closes(&mut reg).is_empty(), "one kick, one request");
    assert_eq!(r.disconnects(), 0, "and one policy call");
}
