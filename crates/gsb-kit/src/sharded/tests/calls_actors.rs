//! The call table on the kit's shard seam path, on REAL shard actors
//! (BACKLOG B168, B152): a game whose `systems_seam_with_calls` takes a
//! client request, calls a bank (`.answers` the client) and walks the
//! player across the seam in the same tick. The call rides the
//! migration: the receiving shard re-issues it under its original key,
//! the game there gets the result under the player's NEW entity, the
//! client its answer — and the bank charges once.
//!
//! A live registry over four shards on a 2×2 grid, a global ticker on a
//! paused clock; the step barrier is the metrics channel (one sample per
//! shard and step, after its broadcast phase — the area rig's).

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::Duration;

use bevy_ecs::prelude::{Entity, World};
use gsb_core::calls::{
    Ask, BootError, CallKey, CallSpec, Calls, Envelope, Mail, Owner, Request, Route,
};
use gsb_core::channel::{FrameBatch, Inbox, Mailbox, channel};
use gsb_core::id::{ConnectionId, PlayerId, RoomId};
use gsb_core::metrics::{CallCounts, MetricsEvent};
use gsb_core::registry::{BuiltRoom, Registry, RegistryMsg, RoomFactory, Seat};
use gsb_core::room::{Action, RoomConfig, TickCtx};
use gsb_core::shard::ShardLogic;
use gsb_core::ticker::Ticker;
use prost::Message;
use tokio::sync::oneshot;

use crate::game::{Game, InputSeq, ShardGame};
use crate::sharded::{KitMig, Seam, ShardedRoom};
use crate::space::{GridPartition2, Partition};
use crate::testing::{DEFAULT_SPEED, FixCodec, FixMig, Fixture, Position, Speed, WirePos};

const HALF: f32 = 100.0;
const SHARDS: usize = 4;
const ROOM: RoomId = RoomId(1);
const WAIT: Duration = Duration::from_secs(30);

// ---- the contract ------------------------------------------------------------

#[derive(Clone, PartialEq, prost::Message)]
struct Buy {
    #[prost(uint64, tag = "1")]
    price: u64,
}
#[derive(Clone, PartialEq, prost::Message)]
struct Bought {
    #[prost(uint64, tag = "1")]
    balance: u64,
}
impl gsb_core::calls::Call for Buy {
    type Response = Bought;
    type Error = Bought;
}
impl Request for Buy {
    const OP: u16 = 1180;
}

#[derive(Clone, Debug)]
struct Charge {
    amount: u64,
}
impl gsb_core::calls::Call for Charge {
    type Response = Bought;
    type Error = Bought;
}
enum BankMsg {
    Charge(Ask<Charge>),
}

/// A fake bank the rig drains after every step: ONE effect per key.
struct Bank {
    inbox: Inbox<Mail<BankMsg>>,
    done: BTreeMap<CallKey, u64>,
    balance: u64,
    charges: u32,
    seen: Vec<CallKey>,
}

impl Bank {
    fn step(&mut self) {
        while let Ok(Mail::Call(Envelope { cx, msg })) = self.inbox.try_recv() {
            let BankMsg::Charge(ask) = msg;
            let key = cx.key();
            self.seen.push(key);
            let balance = *self.done.entry(key).or_insert_with(|| {
                self.balance -= ask.call.amount;
                self.charges += 1;
                self.balance
            });
            ask.answer(Ok(Bought { balance }));
        }
    }
}

// ---- the game ------------------------------------------------------------------

/// What a shard's game saw resolve: (shard, owner, reserve, balance).
#[derive(Debug, Clone, PartialEq)]
struct Resolved {
    shard: usize,
    owner: Owner,
    reserve: u64,
    balance: Option<u64>,
}

/// The fixture game with identity spawns (`x:y`), entity owners, the
/// bank call and the walk across the seam.
struct Paying {
    game: Fixture,
    index: usize,
    bank: Route<BankMsg>,
    feed: Mailbox<Resolved>,
}

fn parse(identity: &str) -> Position {
    let (x, y) = identity.split_once(':').expect("x:y");
    Position {
        x: x.parse().expect("x"),
        y: y.parse().expect("y"),
    }
}

fn entity_of(owner: Owner) -> Option<Entity> {
    Entity::try_from_bits(owner.to_bits())
}

impl Game for Paying {
    type Codec = FixCodec;
    const SNAPSHOT_OP: u16 = Fixture::SNAPSHOT_OP;
    const PRIVATE_OP: u16 = Fixture::PRIVATE_OP;
    fn codec(&self) -> &FixCodec {
        self.game.codec()
    }
    fn spawn_player(&mut self, world: &mut World, conn: ConnectionId) -> Entity {
        self.game.spawn_player(world, conn)
    }
    fn spawn_player_as(&mut self, world: &mut World, _: ConnectionId, identity: &str) -> Entity {
        world.spawn((parse(identity), Speed(DEFAULT_SPEED))).id()
    }
    fn ingest(
        &mut self,
        _world: &mut World,
        _ctx: &TickCtx,
        actions: &mut Vec<Action>,
        _players: &HashMap<PlayerId, Entity>,
        _seq: &mut InputSeq,
    ) {
        actions.clear();
    }
    fn systems(&mut self, _world: &mut World, _ctx: &TickCtx) {}
    fn boot_calls(&mut self, calls: &mut Calls) -> Result<(), BootError> {
        calls.accept::<Buy>()
    }
    /// The player's owner is its entity HERE: a different value on every
    /// shard it crosses.
    fn call_owner(
        &self,
        _world: &World,
        player: PlayerId,
        entity: Option<Entity>,
    ) -> Option<Owner> {
        Some(entity.map_or(player.into(), |e| Owner::from_bits(e.to_bits())))
    }
    fn owner_alive(&self, world: &World, owner: Owner) -> Option<bool> {
        Some(entity_of(owner).is_some_and(|e| world.get_entity(e).is_ok()))
    }
}

impl ShardGame for Paying {
    type Mig = FixMig;
    fn capture(&self, world: &World, entity: Entity) -> FixMig {
        self.game.capture(world, entity)
    }
    fn restore(&mut self, world: &mut World, mig: FixMig) -> Entity {
        self.game.restore(world, mig)
    }
    fn systems_seam_with_calls(
        &mut self,
        world: &mut World,
        _ctx: &TickCtx,
        _seam: &mut Seam<'_, '_, WirePos>,
        calls: &mut Calls,
    ) {
        for r in calls.take_resolved::<Charge, u64>() {
            let resolved = Resolved {
                shard: self.index,
                owner: r.owner,
                reserve: r.ctx,
                balance: r.outcome.ok().map(|b| b.balance),
            };
            self.feed.try_send(resolved).expect("the feed has room");
        }
        for r in calls.take_incoming::<Buy>() {
            let spec = CallSpec::new(
                &self.bank,
                Charge {
                    amount: r.body.price,
                },
                BankMsg::Charge,
            );
            calls
                .call(spec)
                .owner(r.owner)
                .with(r.body.price)
                .answers(r.reply);
            // East across the seam: shard 1's region.
            if let Some(e) = entity_of(r.owner) {
                let mut pos = world.get_mut::<Position>(e).expect("the player's entity");
                pos.x = -pos.x;
            }
        }
    }
}

// ---- the rig -------------------------------------------------------------------

type Factory = RoomFactory<World, (), KitMig<FixMig>, WirePos>;

fn factory(bank: Route<BankMsg>, feed: Mailbox<Resolved>) -> Factory {
    Arc::new(move |_id, _cfg| {
        let partition = || GridPartition2::<Position>::new(SHARDS, HALF);
        let shards = (0..SHARDS)
            .map(|index| {
                let game = Paying {
                    game: Fixture::default(),
                    index,
                    bank: bank.clone(),
                    feed: feed.clone(),
                };
                let room = ShardedRoom::with_game(game, partition(), index);
                let logic: Box<
                    dyn ShardLogic<World, GroupKey = (), State = KitMig<FixMig>, Strip = WirePos>,
                > = Box::new(room);
                (World::new(), logic)
            })
            .collect();
        let grid = partition();
        BuiltRoom::Sharded {
            shards,
            home_shard: Arc::new(move |_conn, identity: &str| {
                Partition::<WirePos>::region_of(&grid, &parse(identity))
            }),
        }
    })
}

struct Rig {
    reg: Mailbox<RegistryMsg>,
    metrics: Inbox<MetricsEvent>,
    feed: Inbox<Resolved>,
    bank: Bank,
    steps: Vec<u64>,
    /// Every shard's latest call counts.
    calls: Vec<CallCounts>,
    resolved: Vec<Resolved>,
    /// The one player: its input and its output.
    player: Option<(Mailbox<Action>, Inbox<FrameBatch>)>,
    /// Every answer the client got: (id, ok, balance).
    answers: Vec<(u64, bool, u64)>,
}

impl Rig {
    async fn new() -> Self {
        let (feed_tx, feed) = channel::<Resolved>(64);
        let (route, inbox) = Route::new("bank", 16);
        let (reg, rx) = channel::<RegistryMsg>(64);
        let (ticker, _task) = Ticker::spawn(30.0, 64).expect("rate");
        let (m_tx, metrics) = channel::<MetricsEvent>(1 << 12);
        let registry = Registry::new(
            rx,
            reg.clone(),
            factory(route, feed_tx),
            ticker,
            m_tx,
            None,
            None,
            None,
        );
        tokio::spawn(registry.run());
        let (reply, created) = oneshot::channel();
        let config = RoomConfig {
            id: ROOM,
            tick_hz: 30.0,
            keepalive_hz: 0.0,
            metrics_cadence_hz: 30.0,
            ..Default::default()
        };
        reg.send(RegistryMsg::CreateRoom { config, reply })
            .await
            .expect("registry");
        created.await.expect("reply").expect("created");
        Self {
            reg,
            metrics,
            feed,
            bank: Bank {
                inbox,
                done: BTreeMap::new(),
                balance: 500,
                charges: 0,
                seen: Vec::new(),
            },
            steps: vec![0; SHARDS],
            calls: vec![CallCounts::default(); SHARDS],
            resolved: Vec::new(),
            player: None,
            answers: Vec::new(),
        }
    }

    async fn join(&mut self, identity: &str) {
        let (out_tx, out) = channel::<FrameBatch>(1024);
        let (reply, joined) = oneshot::channel();
        let msg = RegistryMsg::SpawnPlayer {
            conn: ConnectionId(1),
            room: ROOM,
            out: out_tx,
            identity: identity.to_string(),
            reply,
            claims: None,
        };
        self.reg.send(msg).await.expect("registry");
        let Seat { actions, .. } = tokio::time::timeout(WAIT, joined)
            .await
            .expect("joined in time")
            .expect("reply")
            .expect("accepted");
        self.player = Some((actions, out));
        self.step().await;
    }

    fn buy(&self, id: u64, price: u64) {
        let env = gsb_protocol::base::RpcRequest {
            id,
            op: u32::from(Buy::OP),
            payload: Buy { price }.encode_to_vec(),
        };
        let (actions, _) = self.player.as_ref().expect("joined");
        actions
            .try_send(Action {
                conn: ConnectionId(1),
                player: PlayerId(0),
                op: gsb_core::rpc::RPC_REQ_OP,
                payload: env.encode_to_vec().into(),
            })
            .expect("room");
    }

    /// One global tick: every shard finished it; then the bank drains.
    async fn step(&mut self) {
        let target = self.steps.iter().copied().max().unwrap_or(0) + 1;
        loop {
            while let Ok(event) = self.metrics.try_recv() {
                self.count(event);
            }
            if self.steps.iter().all(|&s| s >= target) {
                break;
            }
            let event = tokio::time::timeout(WAIT, self.metrics.recv())
                .await
                .expect("a sample in time")
                .expect("metrics open");
            self.count(event);
        }
        while let Ok(r) = self.feed.try_recv() {
            self.resolved.push(r);
        }
        if let Some((_, out)) = &mut self.player {
            while let Ok(batch) = out.try_recv() {
                for f in batch.iter().filter(|f| f.op == Fixture::PRIVATE_OP) {
                    let private = crate::proto::Private::decode(&f.payload[..]).expect("Private");
                    for r in private.responses {
                        let balance = Bought::decode(&r.payload[..]).map_or(0, |b| b.balance);
                        self.answers.push((r.id, r.ok, balance));
                    }
                }
            }
        }
        self.bank.step();
    }

    fn count(&mut self, event: MetricsEvent) {
        if let MetricsEvent::Room(s) = event {
            let i = (s.room.0 & 0xffff) as usize;
            self.steps[i] = self.steps[i].max(s.steps);
            self.calls[i] = s.calls;
        }
    }
}

#[tokio::test(start_paused = true)]
async fn a_call_rides_the_players_crossing_on_the_kit_seam_path() {
    let mut rig = Rig::new().await;
    rig.join("-5:-5").await; // shard 0
    rig.steps_until_quiet().await;
    rig.buy(7, 100);
    for _ in 0..6 {
        rig.step().await;
    }

    assert_eq!(rig.bank.charges, 1, "one effect");
    assert_eq!(rig.bank.seen.len(), 2, "issued on shard 0, re-issued on 1");
    assert_eq!(rig.bank.seen[0], rig.bank.seen[1], "under one key");
    assert_eq!(rig.answers, [(7, true, 400)], "the client, answered once");
    assert_eq!(rig.resolved.len(), 1, "{:?}", rig.resolved);
    let r = &rig.resolved[0];
    assert_eq!((r.shard, r.reserve, r.balance), (1, 100, Some(400)));
    assert_ne!(
        r.owner,
        Owner::from(PlayerId(1)),
        "the owner is the entity on shard 1"
    );

    let (c0, c1) = (rig.calls[0], rig.calls[1]);
    assert_eq!(
        (c0.calls_sent, c0.calls_exported, c0.results_late),
        (1, 1, 1)
    );
    assert_eq!((c0.results_ok, c0.answers_sent), (0, 0));
    assert_eq!(
        (
            c1.calls_imported,
            c1.calls_sent,
            c1.results_ok,
            c1.responses_delivered,
            c1.answers_sent,
            c1.responses_owner_gone
        ),
        (1, 1, 1, 1, 1, 0)
    );
}

impl Rig {
    /// A few steps for the join to settle (the kit's first frames).
    async fn steps_until_quiet(&mut self) {
        for _ in 0..3 {
            self.step().await;
        }
    }
}
