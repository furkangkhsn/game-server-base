//! The migration fence on REAL shard actors (round e3, `docs/EVENTS.md`
//! §7.3): three shards, each on its OWN manual ticker so a test picks
//! the worst interleaving (a lagging source, a target that ticks first),
//! the router's real routing table (`EventRoutes`) over a one-row stand-in
//! for the registry's tables — the shards' `Relocated` reports are applied
//! the way the registry applies them (`registry/actor/events.rs`).
//!
//! The step barrier is the metrics channel: one sample per shard and
//! step, after its whole step (MIGRATE included).

use super::*;
use crate::calls::{BootError, Calls, Identity};
use crate::conn::ConnIn;
use crate::events::route::{EventRoutes, Live, Locate, Located, Place};
use crate::events::{
    ClientEvent, Dest, Event, EventIn, Events, Push, PublisherId, Target, WorldEvent,
};
use crate::metrics::EventCounts;
use crate::registry::RegistryMsg;
use prost::Message;
use std::sync::mpsc as smpsc;

const SHARDS: usize = 3;
const ROOM: RoomId = RoomId(5);
/// The install generation the shards are stamped with.
const GENERATION: u64 = 1;
const WAIT: Duration = Duration::from_secs(5);
const BOB: PlayerId = PlayerId(1);

#[derive(Clone, PartialEq, prost::Message)]
struct Gold {
    #[prost(uint64, tag = "1")]
    n: u64,
}
impl Event for Gold {
    const OP: u16 = 2001;
    const TO: Dest = Dest::World;
}
impl WorldEvent for Gold {}

/// A BOTH event: a WORLD leg for the shard, a CLIENT leg for the client.
#[derive(Clone, PartialEq, prost::Message)]
struct Credit {
    #[prost(uint64, tag = "1")]
    n: u64,
}
impl Event for Credit {
    const OP: u16 = 2005;
    const TO: Dest = Dest::Both;
}
impl WorldEvent for Credit {}
impl ClientEvent for Credit {}

/// One event a shard's game took: (shard, op, seq).
type Took = (usize, u16, u64);

/// A shard's logic: players move to the shard the test names (a command
/// channel the MIGRATE phase reads), every event taken is reported.
struct FLogic {
    index: usize,
    serial: u64,
    neighbors: Vec<usize>,
    moves: smpsc::Receiver<(PlayerId, usize)>,
    pending: HashMap<PlayerId, usize>,
    ent: HashMap<PlayerId, u64>,
    players: HashMap<u64, PlayerId>,
    took: smpsc::Sender<Took>,
}

impl GameLogic<()> for FLogic {
    type GroupKey = ();
    type Strip = ();
    fn snapshot_op(&self) -> u16 {
        0x7E30
    }
    fn private_op(&self) -> u16 {
        0x7E31
    }
    fn group_of(&self, _w: &(), _p: PlayerId) {}
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
        self.serial += 1;
        let wire = self.index as u64 * 1_000 + self.serial;
        let player = PlayerId(conn.0);
        self.ent.insert(player, wire);
        self.players.insert(wire, player);
        Admission {
            player,
            entity: wire,
        }
    }
    fn on_leave(&mut self, _w: &mut (), p: PlayerId) {
        if let Some(wire) = self.ent.remove(&p) {
            self.players.remove(&wire);
        }
    }
    fn ingest(&mut self, _w: &mut (), _c: &TickCtx, a: &mut Vec<Action>) {
        a.clear();
    }
    fn update(&mut self, _w: &mut (), _c: &TickCtx) {}
    fn boot_events(&mut self, events: &mut Events) -> Result<(), BootError> {
        events.accept::<Gold>()?;
        events.accept::<Credit>()
    }
}

impl ShardLogic<()> for FLogic {
    type State = ();
    fn index(&self) -> usize {
        self.index
    }
    fn shard_count(&self) -> usize {
        SHARDS
    }
    fn serial_capacity(&self) -> u64 {
        1_000
    }
    fn serial_used(&self) -> u64 {
        self.serial
    }
    fn neighbors(&self) -> &[usize] {
        &self.neighbors
    }
    fn collect_migrations(&mut self, _w: &mut (), nb: usize) -> Vec<Migrating<()>> {
        while let Ok((p, to)) = self.moves.try_recv() {
            self.pending.insert(p, to);
        }
        let go: Vec<PlayerId> = self
            .pending
            .iter()
            .filter(|(_, to)| **to == nb)
            .map(|(p, _)| *p)
            .collect();
        go.into_iter()
            .filter_map(|p| {
                self.pending.remove(&p);
                Some(Migrating {
                    wire: *self.ent.get(&p)?,
                    state: (),
                    player: Some(p),
                })
            })
            .collect()
    }
    fn on_migrate_in(&mut self, _w: &mut (), wire: u64, _s: (), player: Option<PlayerId>) {
        if let Some(p) = player {
            self.ent.insert(p, wire);
            self.players.insert(wire, p);
        }
    }
    fn on_migrate_out(&mut self, _w: &mut (), wire: u64) {
        if let Some(p) = self.players.remove(&wire) {
            self.ent.remove(&p);
        }
    }
    fn collect_border(&self, _w: &()) -> Vec<BorderRecord<()>> {
        Vec::new()
    }
    fn own_wires(&self, _w: &()) -> Vec<u64> {
        self.players.keys().copied().collect()
    }
    fn update_seam_with_events(
        &mut self,
        _w: &mut (),
        _c: &TickCtx,
        _seam: &mut CrossSeam<'_, ()>,
        _calls: &mut Calls,
        events: &mut Events,
    ) {
        for e in events.take_all() {
            let _ = self.took.send((self.index, e.op(), e.seq()));
        }
    }
}

/// The registry's tables for the one identity the tests route ("bob").
struct At {
    /// Its shard (`None`: in no room).
    place: Option<usize>,
    /// The highest migration count applied.
    mig: u64,
    doors: Vec<Mailbox<EventIn>>,
    client: Mailbox<ConnIn>,
}

impl Locate for At {
    fn locate(&self, identity: &str) -> Option<Located<'_>> {
        (identity == "bob").then(|| Located {
            live: Some(Live {
                inbox: &self.client,
                features: gsb_protocol::FEATURE_PUSH,
            }),
            place: match self.place {
                None => Place::Nowhere,
                Some(s) => Place::In(self.doors.get(s)),
            },
        })
    }
    fn members(&self, _room: RoomId) -> Vec<String> {
        vec!["bob".into()]
    }
    fn identities(&self) -> Vec<String> {
        vec!["bob".into()]
    }
    fn doors(&self, _room: RoomId) -> Option<Vec<&Mailbox<EventIn>>> {
        Some(self.doors.iter().collect())
    }
    fn rooms(&self) -> Vec<RoomId> {
        vec![ROOM]
    }
}

struct Lane {
    tick: broadcast::Sender<TickInfo>,
    mailbox: Mailbox<ShardMsg<(), ()>>,
    moves: smpsc::Sender<(PlayerId, usize)>,
    /// The ticks this shard was given.
    t: u64,
    counts: EventCounts,
}

/// Three real shard actors, a router, a client inbox.
struct Rig {
    lanes: Vec<Lane>,
    metrics: mpsc::Receiver<MetricsEvent>,
    reg: Inbox<RegistryMsg>,
    /// The registry mailbox the shards report to (a test fills it to
    /// refuse a `Relocated`).
    reg_tx: Mailbox<RegistryMsg>,
    took: smpsc::Receiver<Took>,
    routes: EventRoutes,
    at: At,
    client: Inbox<ConnIn>,
    /// Ignore every `Relocated` (the router never learns the move).
    deaf: bool,
}

impl Rig {
    fn new(reg_capacity: usize) -> Self {
        let (reg_tx, reg) = crate::channel::channel::<RegistryMsg>(reg_capacity);
        let (m_tx, metrics) = mpsc::channel(4096);
        let (took_tx, took) = smpsc::channel();
        let (client_tx, client) = crate::channel::channel::<ConnIn>(1024);
        let mut txs = Vec::new();
        let mut rxs = Vec::new();
        for _ in 0..SHARDS {
            let (tx, rx) = channel::<ShardMsg<(), ()>>(1024);
            txs.push(tx);
            rxs.push(rx);
        }
        let cfg = RoomConfig {
            id: ROOM,
            tick_hz: 30.0,
            keepalive_hz: 0.0,
            metrics_cadence_hz: 30.0,
            ..Default::default()
        };
        let mut lanes = Vec::new();
        let mut doors = Vec::new();
        for (index, rx) in rxs.into_iter().enumerate() {
            let (tick, tick_rx) = broadcast::channel(64);
            let (moves_tx, moves) = smpsc::channel();
            let logic = FLogic {
                index,
                serial: 0,
                neighbors: (0..SHARDS).filter(|&j| j != index).collect(),
                moves,
                pending: HashMap::new(),
                ent: HashMap::new(),
                players: HashMap::new(),
                took: took_tx.clone(),
            };
            let actor = ShardActor::new(
                cfg.clone(),
                index,
                (),
                Box::new(logic) as Box<dyn ShardLogic<(), GroupKey = (), State = (), Strip = ()>>,
                tick_rx,
                rx,
                txs.clone(),
                1,
                m_tx.clone(),
                None,
            )
            .with_registry(reg_tx.clone())
            .with_effect_epoch(GENERATION);
            doors.push(actor.event_mailbox());
            tokio::spawn(actor.run());
            lanes.push(Lane {
                tick,
                mailbox: txs[index].clone(),
                moves: moves_tx,
                t: 0,
                counts: EventCounts::default(),
            });
        }
        Self {
            lanes,
            metrics,
            reg,
            reg_tx,
            took,
            routes: EventRoutes::default(),
            at: At {
                place: None,
                mig: 0,
                doors,
                client: client_tx,
            },
            client,
            deaf: false,
        }
    }

    /// Shard `i` runs its next tick; waits for its whole step.
    async fn tick(&mut self, i: usize) {
        self.lanes[i].t += 1;
        let t = self.lanes[i].t;
        self.lanes[i]
            .tick
            .send(TickInfo {
                tick: t,
                at: crate::ticker::now(),
            })
            .expect("the shard listens");
        let id = crate::shard::sample_id(ROOM, i);
        loop {
            let ev = tokio::time::timeout(WAIT, self.metrics.recv())
                .await
                .expect("a sample in time")
                .expect("metrics open");
            if let MetricsEvent::Room(s) = ev {
                let j = (s.room.0 & 0xffff) as usize;
                self.lanes[j].counts = s.events;
                if s.room == id && s.steps == t {
                    return;
                }
            }
        }
    }

    /// Every shard once, in this order.
    async fn round(&mut self, order: [usize; SHARDS]) {
        for i in order {
            self.tick(i).await;
        }
    }

    /// "bob" joins shard `i`.
    async fn join(&mut self, i: usize) {
        let (reply, joined) = oneshot::channel();
        let (out, _out_rx) = mpsc::channel(64);
        self.lanes[i]
            .mailbox
            .send(ShardMsg::Join {
                conn: ConnectionId(BOB.0),
                epoch: 1,
                identity: "bob".into(),
                claims: None,
                out,
                reply,
            })
            .await
            .expect("shard open");
        self.tick(i).await;
        joined.await.expect("reply").expect("joined");
        self.at.place = Some(i);
        // The others catch up: every shard at the same tick.
        for j in (0..SHARDS).filter(|&j| j != i) {
            self.tick(j).await;
        }
    }

    /// Shard `from` moves bob to shard `to` in its next MIGRATE.
    fn moves(&self, from: usize, to: usize) {
        self.lanes[from].moves.send((BOB, to)).expect("logic alive");
    }

    /// The publisher pushes `n` events of `E` to bob (routed at once).
    fn push<E: Event + Default + Message>(&mut self, n: usize) {
        for _ in 0..n {
            let p = Push {
                publisher: PublisherId(1),
                to: Target::One(Identity::new("bob")),
                op: E::OP,
                dest: E::TO,
                body: E::default().encode_to_vec().into(),
            };
            self.routes.route(p, &self.at);
        }
    }

    /// The router reads its mailbox: each `Relocated`, applied as the
    /// registry applies it (`Registry::on_relocated`).
    fn pump(&mut self) {
        while let Ok(m) = self.reg.try_recv() {
            let RegistryMsg::Relocated {
                identity,
                shard,
                mig,
                generation,
                ..
            } = m
            else {
                continue;
            };
            if self.deaf || generation != GENERATION || mig <= self.at.mig {
                self.routes.relocation_stale();
                continue;
            }
            let old = self.at.place.replace(shard);
            self.at.mig = mig;
            let door = old.filter(|&o| o != shard).map(|o| &self.at.doors[o]);
            self.routes.relocated(door, &identity, mig);
        }
    }

    /// What the shards took so far: the seqs of `op` per shard, and the
    /// order across shards.
    fn took(&self, op: u16) -> Vec<(usize, u64)> {
        let mut out = Vec::new();
        while let Ok((shard, o, seq)) = self.took.try_recv() {
            if o == op {
                out.push((shard, seq));
            }
        }
        out
    }

    /// The CLIENT legs' seqs so far.
    fn client_seqs(&mut self) -> Vec<u64> {
        let mut out = Vec::new();
        while let Ok(m) = self.client.try_recv() {
            if let ConnIn::Push { seq, .. } = m {
                out.push(seq);
            }
        }
        out
    }

    fn counts(&self, i: usize) -> EventCounts {
        self.lanes[i].counts
    }

    /// Every shard's counts, summed.
    fn total(&self) -> EventCounts {
        let mut c = EventCounts::default();
        for l in &self.lanes {
            c.add(&l.counts);
        }
        c
    }
}

/// Bob joined shard 0, every shard at the same tick.
async fn rig() -> Rig {
    let mut rig = Rig::new(64);
    rig.join(0).await;
    rig.round([1, 2, 0]).await;
    rig
}

/// The hazard, staged on real actors: legs queued at the source after the
/// member left, the router learns the move and sends direct to the
/// target, and the target ticks (twice) before the lagging source
/// forwards. With the fence the target takes 1..=6 in order — without
/// it (mutation: no hold) 4, 5, 6, 1, 2, 3.
async fn single_migration<E: Event + Default + Message>() -> Rig {
    let mut rig = rig().await;
    rig.moves(0, 1);
    rig.tick(0).await; // MIGRATE: bob → shard 1 (tombstone)
    rig.push::<E>(3); // 1..3 → shard 0, which no longer holds bob
    rig.tick(1).await; // the Migrate waits for its install tick
    rig.tick(1).await; // installed; Relocated; hold
    rig.pump(); // place = 1; SEAL → shard 0, behind 1..3
    assert_eq!(rig.at.place, Some(1));
    rig.push::<E>(3); // 4..6 DIRECT to shard 1
    rig.tick(1).await; // the direct legs first: held
    rig.tick(0).await; // the source: 1..3 forwarded, the seal → fence
    rig.tick(1).await; // forwards, fence, release
    rig.tick(1).await;
    rig
}

#[tokio::test]
async fn a_migration_keeps_per_publisher_fifo_with_the_fence() {
    let rig = single_migration::<Gold>().await;
    let took = rig.took(Gold::OP);
    assert_eq!(
        took,
        [(1, 1), (1, 2), (1, 3), (1, 4), (1, 5), (1, 6)],
        "every leg at the new shard, in order"
    );
    let (a, b) = (rig.counts(0), rig.counts(1));
    assert_eq!((a.events_forwarded, a.events_world_not_here), (3, 0));
    assert_eq!(
        (
            b.events_fence_held,
            b.events_fence_released,
            b.events_fence_timeouts,
            b.events_fence_lost
        ),
        (3, 3, 0, 0)
    );
    let r = rig.routes.counts();
    assert_eq!(
        (
            r.events_relocations,
            r.events_seals_sent,
            r.events_relocations_stale
        ),
        (1, 1, 0)
    );
    // The ledger, over the room's shards: every leg sent ends once (a
    // forwarded one where it was handed to).
    let c = rig.total();
    assert_eq!(
        r.events_world_sent,
        c.events_admitted
            + c.events_no_handler
            + c.events_undecodable
            + c.events_unbound
            + c.events_world_not_here
            + c.events_forward_refused
            + c.events_fence_overflow
    );
}

/// A BOTH event: its WORLD leg rides the fence; its CLIENT leg goes
/// straight to the connection, untouched by the move, 1..6.
#[tokio::test]
async fn a_migration_leaves_the_client_leg_alone() {
    let mut rig = single_migration::<Credit>().await;
    let took = rig.took(Credit::OP);
    let seqs: Vec<u64> = took.iter().map(|t| t.1).collect();
    assert_eq!(seqs, [1, 2, 3, 4, 5, 6]);
    assert!(took.iter().all(|t| t.0 == 1));
    assert_eq!(rig.client_seqs(), [1, 2, 3, 4, 5, 6]);
}

/// A → B → C before A's fence reached B, every round in the worst order
/// (C, then B, then A): C takes 1..5. B keeps its OWN direct legs (3, 4)
/// pending behind A's fence and relays A's forwarded ones at once (the
/// chain rule); the first draft — B hands its held legs on as it leaves
/// — makes C take 3, 4 before 1, 2.
#[tokio::test]
async fn a_chained_migration_keeps_fifo_in_the_worst_order() {
    let mut rig = rig().await;
    rig.moves(0, 1);
    rig.tick(0).await; // A → B (mig 1)
    rig.push::<Gold>(2); // 1, 2 → A
    rig.tick(1).await;
    rig.tick(1).await; // B: installed, hold
    rig.pump(); // place B; seal A
    rig.push::<Gold>(2); // 3, 4 → B direct
    rig.moves(1, 2);
    rig.tick(1).await; // B holds 3, 4 — and moves bob on to C (mig 2)
    for _ in 0..4 {
        rig.tick(2).await; // C: installed (mig 2) at its first tick past B's
    }
    rig.pump(); // place C; seal B
    assert_eq!(rig.at.place, Some(2));
    rig.push::<Gold>(1); // 5 → C direct
    for _ in 0..4 {
        rig.round([2, 1, 0]).await;
    }
    let seqs: Vec<(usize, u64)> = rig.took(Gold::OP);
    assert_eq!(seqs, [(2, 1), (2, 2), (2, 3), (2, 4), (2, 5)]);
    for i in 0..SHARDS {
        let c = rig.counts(i);
        assert_eq!(
            (
                c.events_fence_timeouts,
                c.events_fence_lost,
                c.events_world_not_here,
                c.events_fence_overflow
            ),
            (0, 0, 0, 0),
            "shard {i}"
        );
    }
    let r = rig.routes.counts();
    assert_eq!((r.events_relocations, r.events_seals_sent), (2, 2));
}

/// The router never applies the move (a stale `Relocated`): no seal, so
/// no fence — the target's hold releases after `FENCE_TTL` ticks
/// (`events_fence_timeouts`), the source's tombstone expires after
/// `TOMB_TTL` (`events_tombstones_expired`) and a leg still routed there
/// afterwards is `events_world_not_here`. Every one counted.
#[tokio::test]
async fn a_fence_that_never_comes_times_out_and_is_counted() {
    let mut rig = rig().await;
    rig.deaf = true;
    rig.moves(0, 1);
    rig.tick(0).await;
    rig.tick(1).await;
    rig.tick(1).await; // installed, hold — the report goes unheard
    rig.pump();
    assert_eq!(rig.at.place, Some(0), "the router still routes to shard 0");
    rig.push::<Gold>(2); // → shard 0's tombstone → forwarded
    for _ in 0..crate::events::FENCE_TTL + 2 {
        rig.round([0, 1, 2]).await;
    }
    assert_eq!(rig.took(Gold::OP), [(1, 1), (1, 2)], "forwarded, in order");
    assert_eq!(rig.counts(1).events_fence_timeouts, 1);
    assert_eq!(rig.counts(0).events_tombstones_expired, 1);
    rig.push::<Gold>(1); // after the tombstone: nowhere
    rig.round([0, 1, 2]).await;
    assert_eq!(rig.counts(0).events_world_not_here, 1);
    assert_eq!(rig.routes.counts().events_relocations_stale, 1);
}

/// The registry's full mailbox refuses the `Relocated`: the target opens
/// no hold (`events_relocations_refused`); the router keeps routing to
/// the source, whose tombstone hands every leg on — nothing lost.
#[tokio::test]
async fn a_refused_relocation_leaves_the_tombstone_forwarding() {
    let mut rig = Rig::new(1);
    rig.join(0).await;
    // Fill the registry's one slot.
    let filler = RegistryMsg::Push(Push {
        publisher: PublisherId(9),
        to: Target::All,
        op: Gold::OP,
        dest: Dest::World,
        body: bytes::Bytes::new(),
    });
    rig.reg_tx.try_send(filler).expect("one slot");
    rig.moves(0, 1);
    rig.tick(0).await;
    rig.tick(1).await;
    rig.tick(1).await; // installed: Relocated refused
    assert_eq!(rig.counts(1).events_relocations_refused, 1);
    rig.push::<Gold>(3);
    rig.round([1, 0, 2]).await;
    rig.round([1, 0, 2]).await;
    assert_eq!(rig.took(Gold::OP), [(1, 1), (1, 2), (1, 3)]);
    assert_eq!(rig.counts(0).events_forwarded, 3);
    assert_eq!(rig.counts(1).events_fence_held, 0, "no hold");
}

/// A hold keeps at most `HOLD_CAP` legs; the rest are dropped and
/// counted `events_fence_overflow` — and a seal or fence nobody waits
/// for is `events_fence_lost`.
#[tokio::test]
async fn a_full_hold_and_a_stray_seal_are_counted() {
    let mut rig = rig().await;
    rig.moves(0, 1);
    rig.tick(0).await;
    rig.tick(1).await;
    rig.tick(1).await;
    rig.pump();
    let n = crate::events::HOLD_CAP + 5;
    rig.push::<Gold>(n); // direct to shard 1, held
    rig.tick(1).await;
    assert_eq!(rig.counts(1).events_fence_held, crate::events::HOLD_CAP as u64);
    assert_eq!(rig.counts(1).events_fence_overflow, 5);
    rig.tick(0).await; // the fence
    rig.tick(1).await;
    assert_eq!(rig.took(Gold::OP).len(), crate::events::HOLD_CAP);
    // A seal for a migration shard 2 never saw.
    let stray = EventIn::Seal {
        identity: "bob".into(),
        mig: 7,
    };
    rig.at.doors[2].try_send(stray).expect("room");
    rig.tick(2).await;
    assert_eq!(rig.counts(2).events_fence_lost, 1);
}

/// A source that forwards before the target installed the member: the
/// `Forward`s sit behind a `Migrate` the install gate deferred, and wait
/// with it — taken, in order, once the member is there.
#[tokio::test]
async fn a_forward_behind_a_deferred_migration_waits_with_it() {
    let mut rig = rig().await;
    rig.moves(0, 1);
    rig.tick(0).await; // A → B at A's tick
    rig.push::<Gold>(3); // → A
    rig.tick(0).await; // A forwards 1..3 behind the Migrate
    rig.tick(1).await; // B: the Migrate waits for its install tick — so do the forwards
    rig.tick(1).await; // installed, then the forwards
    rig.tick(1).await;
    assert_eq!(rig.took(Gold::OP), [(1, 1), (1, 2), (1, 3)]);
    assert_eq!(rig.counts(1).events_world_not_here, 0);
}
