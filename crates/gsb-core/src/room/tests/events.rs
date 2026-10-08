//! The event table in a real room actor (round e2, `docs/EVENTS.md`):
//! WORLD legs put straight into the room's own event mailbox (what the
//! registry's router does — `crates/gsb-core/tests/events.rs` drives the
//! router itself), the room stepped by hand.
//!
//! The game is plain (ECS-free): a wallet per player that a
//! `GoldReceived` event sets (newest version wins) and a `Buy` request
//! spends from.

use super::*;
use crate::calls::{BootError, Calls, Identity, Owner};
use crate::events::{Addressee, Dest, Event, EventIn, Events, PublisherId, WorldEvent, WorldPush};
use crate::room::actor::RoomActor;
use crate::rpc::RPC_REQ_OP;
use prost::Message;
use std::collections::BTreeMap;
use std::sync::mpsc as smpsc;

const OP_SNAP: u16 = 0x7070;
const OP_PRIV: u16 = 0x7071;

// ---- the contract (what `gsb-build` would generate) ------------------------

#[derive(Clone, PartialEq, prost::Message)]
pub(super) struct GoldReceived {
    #[prost(uint64, tag = "1")]
    pub balance: u64,
    #[prost(uint64, tag = "2")]
    pub version: u64,
}
impl Event for GoldReceived {
    const OP: u16 = 2001;
    const TO: Dest = Dest::World;
}
impl WorldEvent for GoldReceived {}

/// A BOTH event (the room takes its WORLD leg).
#[derive(Clone, PartialEq, prost::Message)]
pub(super) struct Banner {
    #[prost(string, tag = "1")]
    pub text: String,
}
impl Event for Banner {
    const OP: u16 = 2005;
    const TO: Dest = Dest::Both;
}
impl WorldEvent for Banner {}

/// A WORLD event this room never accepts.
#[derive(Clone, PartialEq, prost::Message)]
pub(super) struct Unwanted {
    #[prost(uint32, tag = "1")]
    pub n: u32,
}
impl Event for Unwanted {
    const OP: u16 = 2006;
    const TO: Dest = Dest::World;
}
impl WorldEvent for Unwanted {}

#[derive(Clone, PartialEq, prost::Message)]
pub(super) struct Buy {
    #[prost(uint64, tag = "1")]
    pub price: u64,
}
#[derive(Clone, PartialEq, prost::Message)]
pub(super) struct Bought {
    #[prost(uint64, tag = "1")]
    pub gold: u64,
}
#[derive(Clone, PartialEq, prost::Message)]
pub(super) struct Broke {
    #[prost(uint64, tag = "1")]
    pub gold: u64,
}
impl crate::calls::Call for Buy {
    type Response = Bought;
    type Error = Broke;
}
impl crate::calls::Request for Buy {
    const OP: u16 = 1101;
}

// ---- the game ----------------------------------------------------------------

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct Wallet {
    pub gold: u64,
    pub version: u64,
}

/// What the game saw.
#[derive(Debug, Clone, PartialEq)]
pub(super) enum Seen {
    /// A `GoldReceived`: (owner, balance, publisher, seq, addressee).
    Gold(Owner, u64, PublisherId, u64, Addressee),
    /// A `Banner` (room-wide or not).
    Banner(Owner, String, Addressee),
    /// A wallet at the end of a systems run.
    Wallet(PlayerId, Wallet),
}

#[derive(Debug, Clone, Copy)]
pub(super) struct Mode {
    /// Take the events at all (`false`: `events_untaken`).
    pub take: bool,
    /// Take events before requests (the order EVENTS §3.1 recommends).
    pub events_first: bool,
    /// Take through `take_all` (canonical order) instead of per type.
    pub take_all: bool,
    /// Members get no owner (`call_owner` → `None`).
    pub unbound: bool,
    /// Park a disconnected player.
    pub hold: bool,
    /// Bind this session to this owner explicitly at the first systems
    /// run that sees it.
    pub bind: Option<(u64, Owner)>,
    /// Accept `GoldReceived` twice at boot (a collision).
    pub boot_twice: bool,
}

impl Default for Mode {
    fn default() -> Self {
        Self {
            take: true,
            events_first: true,
            take_all: false,
            unbound: false,
            hold: false,
            bind: None,
            boot_twice: false,
        }
    }
}

pub(super) struct Game {
    pub mode: Mode,
    pub wallets: BTreeMap<PlayerId, Wallet>,
    pub parked: BTreeMap<String, PlayerId>,
    pub seen: smpsc::Sender<Seen>,
    /// A new mode, from the test.
    pub modes: smpsc::Receiver<Mode>,
}

impl Game {
    fn events(&mut self, events: &mut Events) {
        if !self.mode.take {
            return;
        }
        if self.mode.take_all {
            for any in events.take_all() {
                match any.downcast::<GoldReceived>() {
                    Ok(e) => self.gold(e),
                    Err(any) => {
                        let e = any.downcast::<Banner>().ok().expect("two types");
                        let _ = self.seen.send(Seen::Banner(e.owner, e.body.text, e.to));
                    }
                }
            }
            return;
        }
        for e in events.take::<GoldReceived>() {
            self.gold(e);
        }
        for e in events.take::<Banner>() {
            let _ = self.seen.send(Seen::Banner(e.owner, e.body.text, e.to));
        }
    }

    fn gold(&mut self, e: crate::events::Arrived<GoldReceived>) {
        let _ = self.seen.send(Seen::Gold(
            e.owner,
            e.body.balance,
            e.from,
            e.seq,
            e.to.clone(),
        ));
        if let Some(w) = self.wallets.get_mut(&PlayerId(e.owner.0))
            && e.body.version > w.version
        {
            *w = Wallet {
                gold: e.body.balance,
                version: e.body.version,
            };
        }
    }

    fn requests(&mut self, calls: &mut Calls) {
        for r in calls.take_incoming::<Buy>() {
            let w = self.wallets.entry(PlayerId(r.owner.0)).or_default();
            if w.gold >= r.body.price {
                w.gold -= r.body.price;
                let gold = w.gold;
                calls.reply(r.reply, Ok(Bought { gold }));
            } else {
                let gold = w.gold;
                calls.reply(r.reply, Err(Broke { gold }));
            }
        }
    }
}

impl GameLogic<()> for Game {
    type GroupKey = ();
    type Strip = ();
    fn snapshot_op(&self) -> u16 {
        OP_SNAP
    }
    fn private_op(&self) -> u16 {
        OP_PRIV
    }
    fn group_of(&self, _w: &(), _p: PlayerId) {}
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
    fn private(
        &mut self,
        _w: &mut (),
        _p: PlayerId,
        _g: &(),
        replies: &[crate::rpc::RpcReply],
        out: &mut bytes::BytesMut,
    ) -> bool {
        for r in replies {
            let mut buf = Vec::new();
            gsb_protocol::base::RpcResponse::from(r)
                .encode_length_delimited(&mut buf)
                .expect("vec grows");
            out.extend_from_slice(&buf);
        }
        !replies.is_empty()
    }
    fn on_join(&mut self, _w: &mut (), c: ConnectionId) -> Admission {
        self.wallets.insert(PlayerId(c.0), Wallet::default());
        Admission {
            player: PlayerId(c.0),
            entity: c.0,
        }
    }
    fn on_leave(&mut self, _w: &mut (), p: PlayerId) {
        self.wallets.remove(&p);
    }
    fn ingest(&mut self, _w: &mut (), _c: &TickCtx, a: &mut Vec<Action>) {
        a.clear();
    }
    fn update(&mut self, _w: &mut (), _c: &TickCtx) {
        panic!("the actor calls update_with_events");
    }
    fn boot_calls(&mut self, calls: &mut Calls) -> Result<(), BootError> {
        calls.accept::<Buy>()
    }
    fn boot_events(&mut self, events: &mut Events) -> Result<(), BootError> {
        events.accept::<GoldReceived>()?;
        events.accept::<Banner>()?;
        if self.mode.boot_twice {
            events.accept::<GoldReceived>()?;
        }
        Ok(())
    }
    fn call_owner(&self, _w: &(), player: PlayerId) -> Option<Owner> {
        (!self.mode.unbound).then_some(player.into())
    }
    fn update_with_calls(&mut self, _w: &mut (), _c: &TickCtx, _calls: &mut Calls) {
        panic!("the actor calls update_with_events");
    }
    fn update_with_events(
        &mut self,
        _w: &mut (),
        _c: &TickCtx,
        calls: &mut Calls,
        events: &mut Events,
    ) {
        while let Ok(m) = self.modes.try_recv() {
            self.mode = m;
        }
        if let Some((conn, owner)) = self.mode.bind
            && self.wallets.contains_key(&PlayerId(conn))
        {
            calls.bind(ConnectionId(conn), owner);
            self.mode.bind = None;
        }
        if self.mode.events_first {
            self.events(events);
            self.requests(calls);
        } else {
            self.requests(calls);
            self.events(events);
        }
        for (p, w) in &self.wallets {
            let _ = self.seen.send(Seen::Wallet(*p, *w));
        }
    }
    fn on_disconnect(
        &mut self,
        _w: &mut (),
        player: PlayerId,
        identity: &str,
    ) -> crate::room::Detach {
        if !self.mode.hold {
            return crate::room::Detach::Despawn;
        }
        self.parked.insert(identity.to_owned(), player);
        crate::room::Detach::Hold {
            grace: Some(Duration::from_secs(60)),
            to: crate::room::ExpireTo::Despawn,
        }
    }
}

impl RoomLogic<()> for Game {}

// ---- the rig ----------------------------------------------------------------

pub(super) struct Rig {
    pub room: RoomActor<(), (), ()>,
    pub seen: smpsc::Receiver<Seen>,
    pub tick: u64,
    /// The room's own event mailbox (the router's end).
    pub door: Mailbox<EventIn>,
    /// The game's mode, from here on.
    pub modes: smpsc::Sender<Mode>,
    /// What the game reported and no call consumed yet.
    pub log: Vec<Seen>,
}

impl Rig {
    pub(super) fn new(mode: Mode) -> Self {
        Self::sized(mode, crate::events::DEFAULT_EVENT_CAPACITY, 256)
    }

    pub(super) fn sized(mode: Mode, event_capacity: usize, action_capacity: usize) -> Self {
        let (tx, seen) = smpsc::channel();
        let (modes, mode_rx) = smpsc::channel();
        let game = Game {
            mode,
            wallets: BTreeMap::new(),
            parked: BTreeMap::new(),
            seen: tx,
            modes: mode_rx,
        };
        let (_tick_tx, tick_rx) = broadcast::channel(8);
        let (_control, control_rx) = channel(16);
        let room = RoomActor::new(
            RoomConfig {
                id: RoomId(78),
                tick_hz: 30.0,
                keepalive_hz: 0.0,
                metrics_cadence_hz: 0.0,
                event_capacity,
                action_capacity,
                ..Default::default()
            },
            (),
            Box::new(game),
            tick_rx,
            control_rx,
            1,
            null_metrics_tx(),
            None,
        );
        let door = room.event_mailbox();
        Self {
            room,
            seen,
            tick: 0,
            door,
            modes,
            log: Vec::new(),
        }
    }

    pub(super) fn join(&mut self, conn: u64, identity: &str) -> Member {
        let (out, rx) = mpsc::channel::<FrameBatch>(64);
        let (reply, mut replied) = oneshot::channel();
        self.room.handle_control(RoomControl::Resume {
            conn: ConnectionId(conn),
            epoch: 0,
            identity: identity.to_owned(),
            claims: None,
            out,
            reply,
        });
        let (_entity, actions) = replied
            .try_recv()
            .expect("reply sent synchronously")
            .expect("join accepted");
        Member {
            conn,
            actions,
            out: rx,
        }
    }

    /// The router's WORLD leg for `to` (`None`: room-wide).
    pub(super) fn push<E: Event>(&self, to: Option<&str>, seq: u64, e: &E) {
        self.push_raw(to, seq, E::OP, e.to_wire());
    }

    pub(super) fn push_raw(&self, to: Option<&str>, seq: u64, op: u16, body: Vec<u8>) {
        let to = match to {
            Some(id) => Addressee::Player(Identity::new(id)),
            None => Addressee::Room,
        };
        self.door
            .try_send(EventIn::Push(WorldPush {
                to,
                publisher: PublisherId(7),
                seq,
                op,
                body: body.into(),
            }))
            .expect("the event mailbox has room");
    }

    pub(super) fn step(&mut self) {
        self.tick += 1;
        let at = Instant::now() + Duration::from_secs_f64(self.tick as f64 / 30.0);
        assert!(self.room.step(&TickInfo {
            tick: self.tick,
            at
        }));
    }

    /// The events the game saw since the last call (wallets aside).
    pub(super) fn seen(&mut self) -> Vec<Seen> {
        self.log.extend(self.seen.try_iter());
        let (wallets, events) = std::mem::take(&mut self.log)
            .into_iter()
            .partition(|s| matches!(s, Seen::Wallet(..)));
        self.log = wallets;
        events
    }

    /// The game's wallet of `p` as last reported.
    pub(super) fn wallet(&mut self, p: u64) -> Option<Wallet> {
        self.log.extend(self.seen.try_iter());
        self.log.iter().rev().find_map(|s| match s {
            Seen::Wallet(q, w) if *q == PlayerId(p) => Some(*w),
            _ => None,
        })
    }

    pub(super) fn counts(&self) -> crate::metrics::EventCounts {
        self.room.events.counts()
    }
}

pub(super) struct Member {
    pub conn: u64,
    pub actions: Mailbox<Action>,
    pub out: mpsc::Receiver<FrameBatch>,
}

impl Member {
    pub(super) fn buy(&self, id: u64, price: u64) {
        let env = gsb_protocol::base::RpcRequest {
            id,
            op: u32::from(<Buy as crate::calls::Request>::OP),
            payload: Buy { price }.encode_to_vec(),
        };
        self.actions
            .try_send(Action {
                conn: ConnectionId(self.conn),
                player: PlayerId(0),
                op: RPC_REQ_OP,
                payload: env.encode_to_vec().into(),
            })
            .expect("input has room");
    }

    /// The answers delivered since the last call: `Ok(gold)` / `Err(gold)`.
    pub(super) fn answers(&mut self) -> Vec<Result<u64, u64>> {
        let mut out = Vec::new();
        while let Ok(batch) = self.out.try_recv() {
            for f in batch.iter().filter(|f| f.op == OP_PRIV) {
                let mut buf = &f.payload[..];
                while !buf.is_empty() {
                    let r = gsb_protocol::base::RpcResponse::decode_length_delimited(&mut buf)
                        .expect("a response");
                    out.push(if r.ok {
                        Ok(Bought::decode(&r.payload[..]).expect("Bought").gold)
                    } else {
                        Err(Broke::decode(&r.payload[..]).expect("Broke").gold)
                    });
                }
            }
        }
        out
    }
}

fn gold(balance: u64, version: u64) -> GoldReceived {
    GoldReceived { balance, version }
}

// ---- the tests ---------------------------------------------------------------

/// EVENTS runs before READ and the systems: gold that arrives this tick
/// pays for a purchase made this tick, when the game takes its events
/// first (EVENTS §3.1). Taking them after the requests (the game's
/// choice) refuses the same purchase — the engine's order makes the
/// difference available, the game decides.
#[test]
fn gold_arriving_this_tick_pays_for_a_purchase_this_tick() {
    for (events_first, want) in [(true, Ok(0)), (false, Err(0))] {
        let mut rig = Rig::new(Mode {
            events_first,
            ..Mode::default()
        });
        let mut a = rig.join(1, "alice");
        rig.step();
        rig.push(Some("alice"), 1, &gold(500, 1));
        a.buy(9, 500);
        rig.step();
        assert_eq!(a.answers(), vec![want], "events first: {events_first}");
        let c = rig.counts();
        assert_eq!((c.events_admitted, c.events_taken), (1, 1));
    }
}

/// The event carries its addressee, the owner the session is bound to
/// (the default: the player), the publisher and the WORLD leg's number;
/// per-type FIFO keeps the order the router sent.
#[test]
fn an_event_reaches_its_owner_in_order() {
    let mut rig = Rig::new(Mode::default());
    let _a = rig.join(1, "alice");
    let _b = rig.join(2, "bob");
    rig.step();
    rig.push(Some("bob"), 1, &gold(10, 1));
    rig.push(Some("alice"), 1, &gold(20, 1));
    rig.push(Some("bob"), 2, &gold(30, 2));
    rig.step();
    let alice = Identity::new("alice");
    let bob = Identity::new("bob");
    assert_eq!(
        rig.seen(),
        vec![
            Seen::Gold(
                Owner(2),
                10,
                PublisherId(7),
                1,
                Addressee::Player(bob.clone())
            ),
            Seen::Gold(Owner(1), 20, PublisherId(7), 1, Addressee::Player(alice)),
            Seen::Gold(Owner(2), 30, PublisherId(7), 2, Addressee::Player(bob)),
        ]
    );
    assert_eq!(
        rig.wallet(2),
        Some(Wallet {
            gold: 30,
            version: 2
        })
    );
}

/// The owner is the call table's: an explicit `calls.bind` wins over
/// `call_owner` (an ECS game's entity).
#[test]
fn the_owner_is_the_sessions_call_binding() {
    let mut rig = Rig::new(Mode {
        bind: Some((1, Owner(0xE17))),
        ..Mode::default()
    });
    let _a = rig.join(1, "alice");
    rig.step();
    rig.push(Some("alice"), 1, &gold(5, 1));
    rig.step();
    assert!(matches!(rig.seen()[..], [Seen::Gold(Owner(0xE17), 5, ..)]));
}

/// A room-wide leg (`push_room`, `push_all`) is owned by the actor.
#[test]
fn a_room_wide_event_is_owned_by_the_actor() {
    let mut rig = Rig::new(Mode::default());
    rig.step();
    rig.push(
        None,
        1,
        &Banner {
            text: "double xp".into(),
        },
    );
    rig.step();
    assert_eq!(
        rig.seen(),
        vec![Seen::Banner(
            Owner::ACTOR,
            "double xp".into(),
            Addressee::Room
        )]
    );
}

/// `take_all` hands every event in the drain's (canonical) order, across
/// types.
#[test]
fn take_all_keeps_the_canonical_order_across_types() {
    let mut rig = Rig::new(Mode {
        take_all: true,
        ..Mode::default()
    });
    let _a = rig.join(1, "alice");
    rig.step();
    rig.push(None, 1, &Banner { text: "a".into() });
    rig.push(Some("alice"), 1, &gold(1, 1));
    rig.push(None, 2, &Banner { text: "b".into() });
    rig.step();
    let order: Vec<String> = rig
        .seen()
        .into_iter()
        .map(|s| match s {
            Seen::Gold(_, b, ..) => format!("gold {b}"),
            Seen::Banner(_, t, _) => t,
            Seen::Wallet(..) => unreachable!("filtered"),
        })
        .collect();
    assert_eq!(order, ["a", "gold 1", "b"]);
    assert_eq!(rig.counts().events_taken, 3);
}

/// What nobody took is dropped at tick end and counted — never carried
/// into the next tick (EVENTS §4.4).
#[test]
fn an_untaken_event_is_dropped_and_counted() {
    let mut rig = Rig::new(Mode {
        take: false,
        ..Mode::default()
    });
    let _a = rig.join(1, "alice");
    rig.step();
    rig.push(Some("alice"), 1, &gold(5, 1));
    rig.step();
    assert_eq!(rig.counts().events_untaken, 1);
    rig.modes.send(Mode::default()).expect("the game listens");
    rig.step();
    assert!(rig.seen().is_empty(), "nothing carried over");
    let c = rig.counts();
    assert_eq!(
        (c.events_admitted, c.events_taken, c.events_untaken),
        (1, 0, 1)
    );
}

/// A parked member is a member: its WORLD legs are delivered (§20 E1) —
/// the bank's balance lands on the parked character.
#[test]
fn a_parked_member_still_gets_its_world_legs() {
    let mut rig = Rig::new(Mode {
        hold: true,
        ..Mode::default()
    });
    let _a = rig.join(1, "alice");
    rig.step();
    rig.room.handle_control(RoomControl::Detach {
        conn: ConnectionId(1),
        entity: 1,
        identity: "alice".into(),
    });
    rig.push(Some("alice"), 1, &gold(77, 1));
    rig.step();
    assert!(matches!(rig.seen()[..], [Seen::Gold(Owner(1), 77, ..)]));
    assert_eq!(
        rig.wallet(1),
        Some(Wallet {
            gold: 77,
            version: 1
        })
    );
}

/// Every WORLD leg the room cannot hand to its game is counted by name,
/// and the counts add up: `sent = admitted + no_handler + undecodable +
/// unbound + world_not_here`, `admitted = taken + untaken`.
#[test]
fn every_room_side_loss_is_counted_and_the_ledger_adds_up() {
    let mut rig = Rig::new(Mode::default());
    let _a = rig.join(1, "alice");
    rig.step();
    rig.push(Some("alice"), 1, &gold(1, 1)); // admitted, taken
    rig.push(Some("carol"), 1, &gold(1, 1)); // not here
    rig.push(Some("alice"), 1, &Unwanted { n: 1 }); // no handler
    rig.push_raw(Some("alice"), 2, GoldReceived::OP, vec![0xFF; 3]); // undecodable
    rig.step();
    let mut unbound = Rig::new(Mode {
        unbound: true,
        ..Mode::default()
    });
    let _b = unbound.join(2, "bob");
    unbound.step();
    unbound.push(Some("bob"), 1, &gold(1, 1)); // unbound
    unbound.step();
    let mut c = rig.counts();
    c.add(&unbound.counts());
    assert_eq!(
        (
            c.events_admitted,
            c.events_world_not_here,
            c.events_no_handler,
            c.events_undecodable,
            c.events_unbound
        ),
        (1, 1, 1, 1, 1)
    );
    let sent = 5;
    assert_eq!(
        c.events_admitted
            + c.events_no_handler
            + c.events_undecodable
            + c.events_unbound
            + c.events_world_not_here,
        sent
    );
    assert_eq!(c.events_admitted, c.events_taken + c.events_untaken);
}

/// The event mailbox is the ROOM's: a member whose input channel is full
/// (a flooding client) still gets every one of its events (EVENTS §3.1).
#[test]
fn a_flooding_member_does_not_lose_its_events() {
    let mut rig = Rig::sized(Mode::default(), 64, 8);
    let a = rig.join(1, "alice");
    rig.step();
    let mut flooded = 0;
    while a
        .actions
        .try_send(Action {
            conn: ConnectionId(1),
            player: PlayerId(0),
            op: 0x0FFF,
            payload: bytes::Bytes::new(),
        })
        .is_ok()
    {
        flooded += 1;
    }
    assert_eq!(flooded, 8, "the input channel is full");
    for v in 1..=50 {
        rig.push(Some("alice"), v, &gold(v, v));
    }
    rig.step();
    assert_eq!(rig.seen().len(), 50);
    assert_eq!(rig.counts().events_taken, 50);
}

/// One EVENTS phase drains at most one mailbox's worth: a router still
/// sending cannot keep the tick there; the rest waits for the next tick.
#[test]
fn one_events_phase_drains_at_most_one_mailbox() {
    let mut rig = Rig::sized(Mode::default(), 4, 256);
    let _a = rig.join(1, "alice");
    rig.step();
    for v in 1..=4 {
        rig.push(Some("alice"), v, &gold(v, v));
    }
    assert!(
        rig.door
            .try_send(EventIn::Push(WorldPush {
                to: Addressee::Room,
                publisher: PublisherId(7),
                seq: 1,
                op: Banner::OP,
                body: bytes::Bytes::new(),
            }))
            .is_err(),
        "a full mailbox refuses (the router counts it)"
    );
    rig.step();
    assert_eq!(rig.counts().events_taken, 4);
}

/// A quiet room's EVENTS costs one empty `try_recv`: after 1000 steps the
/// table never allocated and no counter moved (EVENTS §3.2).
#[test]
fn a_quiet_room_never_allocates_its_event_table() {
    let mut rig = Rig::new(Mode::default());
    let _a = rig.join(1, "alice");
    for _ in 0..1000 {
        rig.step();
    }
    assert_eq!(rig.room.events.table.capacity(), 0);
    assert_eq!(rig.counts(), crate::metrics::EventCounts::default());
}

/// An event op claimed twice refuses the room's boot (the registry does
/// not install it, B153); a directly built actor keeps the error.
#[test]
fn an_event_claimed_twice_is_a_boot_error() {
    let rig = Rig::new(Mode {
        boot_twice: true,
        ..Mode::default()
    });
    assert!(matches!(
        rig.room.boot_error(),
        Some(BootError::EventOpCollision { op: 2001, .. })
    ));
}
