//! The rig of the rejoin race (BACKLOG B43): a live registry whose rooms
//! reach it through a relay that HOLDS every close request — the stand-in
//! for a registry mailbox that stays full for the room's `try_send` while
//! the connection's awaited join still gets through — and a logic that
//! mints a NEW entity on every join, logs its hooks and, if asked, kicks
//! whoever sends game input.

use super::*;

/// Wire ids per shard (`index * SHARD_SPAN + serial`).
const SHARD_SPAN: u64 = 1000;

/// A hook the logic ran.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hook {
    Join(PlayerId, EntityId),
    Leave(PlayerId),
    Disconnect(PlayerId),
}

pub type Hooks = mpsc::UnboundedSender<Hook>;

/// The cause each disconnect was asked with (BACKLOG F27).
pub type Causes = mpsc::UnboundedSender<(PlayerId, DisconnectCause)>;

pub struct RejoinLogic {
    hooks: Hooks,
    index: usize,
    /// Kick the sender of any game input (the E8 verb, from `ingest`).
    kick_on_input: bool,
    /// Joins so far: the next join's serial (never reused).
    joins: u64,
    live: HashMap<PlayerId, EntityId>,
    /// Where to log each disconnect's cause (none: not logged).
    causes: Option<Causes>,
}

impl GameLogic<()> for RejoinLogic {
    type GroupKey = ();
    type Strip = ();
    fn snapshot_op(&self) -> u16 {
        0x7E60
    }
    fn private_op(&self) -> u16 {
        0x7E61
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
    fn on_join(&mut self, _w: &mut (), _c: ConnectionId) -> Admission {
        self.joins += 1;
        let entity = self.index as u64 * SHARD_SPAN + self.joins;
        let player = PlayerId(entity);
        self.live.insert(player, entity);
        let _ = self.hooks.send(Hook::Join(player, entity));
        Admission { player, entity }
    }
    fn on_leave(&mut self, _w: &mut (), player: PlayerId) {
        self.live.remove(&player);
        let _ = self.hooks.send(Hook::Leave(player));
    }
    fn on_disconnect(&mut self, _w: &mut (), player: PlayerId, _identity: &str) -> Detach {
        let _ = self.hooks.send(Hook::Disconnect(player));
        Detach::Despawn
    }
    fn on_disconnect_with(
        &mut self,
        w: &mut (),
        player: PlayerId,
        identity: &str,
        cause: DisconnectCause,
    ) -> Detach {
        if let Some(causes) = &self.causes {
            let _ = causes.send((player, cause));
        }
        self.on_disconnect(w, player, identity)
    }
    fn ingest(&mut self, _w: &mut (), ctx: &TickCtx, a: &mut Vec<Action>) {
        if self.kick_on_input {
            for action in a.iter() {
                ctx.kick(action.player, "cheating");
            }
        }
        a.clear();
    }
    fn update(&mut self, _w: &mut (), _c: &TickCtx) {}
}

impl RoomLogic<()> for RejoinLogic {}

impl ShardLogic<()> for RejoinLogic {
    type State = ();
    fn index(&self) -> usize {
        self.index
    }
    fn shard_count(&self) -> usize {
        2
    }
    fn serial_capacity(&self) -> u64 {
        SHARD_SPAN
    }
    fn serial_used(&self) -> u64 {
        self.joins
    }
    fn neighbors(&self) -> &[usize] {
        &[]
    }
    fn collect_migrations(&mut self, _w: &mut (), _nb: usize) -> Vec<Migrating<()>> {
        Vec::new()
    }
    fn on_migrate_in(&mut self, _w: &mut (), _wire: u64, _s: (), _p: Option<PlayerId>) {}
    fn on_migrate_out(&mut self, _w: &mut (), _wire: u64) {}
    fn collect_border(&self, _w: &()) -> Vec<BorderRecord<()>> {
        Vec::new()
    }
    fn own_wires(&self, _w: &()) -> Vec<u64> {
        self.live.values().copied().collect()
    }
}

/// A factory of `RejoinLogic` rooms (despawn on disconnect): one room, or
/// two shards homing every join to shard 0.
pub fn factory(sharded: bool, kick_on_input: bool, hooks: Hooks) -> RoomFactory<(), (), (), ()> {
    factory_with(sharded, kick_on_input, hooks, None)
}

/// [`factory`] whose rooms also log each disconnect's cause.
pub fn factory_with(
    sharded: bool,
    kick_on_input: bool,
    hooks: Hooks,
    causes: Option<Causes>,
) -> RoomFactory<(), (), (), ()> {
    Arc::new(move |_id, _cfg| {
        let logic = |index: usize| RejoinLogic {
            hooks: hooks.clone(),
            index,
            kick_on_input,
            joins: 0,
            live: HashMap::new(),
            causes: causes.clone(),
        };
        if sharded {
            let shard = |i: usize| {
                (
                    (),
                    Box::new(logic(i))
                        as Box<dyn ShardLogic<(), GroupKey = (), State = (), Strip = ()>>,
                )
            };
            BuiltRoom::Sharded {
                shards: vec![shard(0), shard(1)],
                home_shard: Arc::new(|_c, _i: &str| 0),
            }
        } else {
            BuiltRoom::Single {
                world: (),
                logic: Box::new(logic(0)),
            }
        }
    })
}

/// A live registry hosting room 1 (`config`), whose rooms' requests pass
/// a relay: every message is forwarded in order except the close
/// requests, which land in the returned receiver for the test to hand
/// over (or not) when it chooses. Connections talk to the registry
/// directly. Also returns the metrics receiver.
pub async fn held_registry(
    factory: RoomFactory<(), (), (), ()>,
    config: RoomConfig,
) -> (
    Mailbox<RegistryMsg>,
    mpsc::UnboundedReceiver<CloseRequest>,
    mpsc::Receiver<MetricsEvent>,
) {
    let (tx, rx) = channel::<RegistryMsg>(4096);
    let (relay, mut relayed) = channel::<RegistryMsg>(4096);
    let (held_tx, held) = mpsc::unbounded_channel();
    let forward = tx.clone();
    tokio::spawn(async move {
        while let Some(msg) = relayed.recv().await {
            match msg {
                RegistryMsg::CloseConn(req) => {
                    let _ = held_tx.send(req);
                }
                other => {
                    if forward.send(other).await.is_err() {
                        return;
                    }
                }
            }
        }
    });
    let (ticker, _task) = Ticker::spawn(30.0, 64).expect("valid tick rate");
    let (metrics_tx, metrics) = mpsc::channel::<MetricsEvent>(1 << 16);
    tokio::spawn(Registry::new(rx, relay, factory, ticker, metrics_tx, None, None, None).run());
    create(&tx, config).await;
    (tx, held, metrics)
}

/// The registry's connection-table size: drop the samples so far, open a
/// probe connection (it flushes a fresh sample) and read that sample.
pub async fn table_size(
    tx: &Mailbox<RegistryMsg>,
    metrics: &mut mpsc::Receiver<MetricsEvent>,
) -> u32 {
    while metrics.try_recv().is_ok() {}
    let (inbox, _rx) = mpsc::channel(1);
    tx.send(RegistryMsg::ConnOpened {
        conn: ConnectionId(999),
        inbox,
    })
    .await
    .expect("registry alive");
    loop {
        match tokio::time::timeout(WAIT, metrics.recv()).await {
            Ok(Some(MetricsEvent::Registry(s))) => return s.conns - 1,
            Ok(Some(_)) => {}
            other => panic!("no registry sample: {other:?}"),
        }
    }
}
