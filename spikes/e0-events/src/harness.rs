//! A small server built from the spike's actors, driven step by step
//! (the tests' scaffolding), and the worked examples' room game.
//!
//! In the engine the router's location notices (`Online`, `Joined`,
//! `Parked` …) are facts the registry learns on its own; here the harness
//! sends them where the registry would.

use std::collections::BTreeMap;

use tokio::sync::mpsc;

use crate::client::EventsClient;
use crate::conn::ConnActor;
use crate::contract::{ConnId, Identity, Owner, RoomId};
use crate::events::Events;
use crate::game::{BalanceChanged, GoldReceived};
use crate::record::Fnv;
use crate::room::{Action, Addressee, Caps, Control, Game, Io, Room, RoomHandles};
use crate::router::{ConnLink, Router, RouterMsg};
use crate::wire::Frame;

/// One connected player: its connection actor, the client's end of the
/// outbound, the client's event half, and its input sender.
pub struct Player {
    /// The identity.
    pub id: Identity,
    /// The current session.
    pub conn: ConnId,
    /// The connection actor.
    pub actor: ConnActor,
    /// What the client receives.
    pub frames: mpsc::Receiver<Frame>,
    /// The client's event half.
    pub client: EventsClient,
    /// The client's input channel into its room (READ).
    pub input: Option<mpsc::Sender<Action>>,
    features: u64,
    where_: Option<(RoomId, u16)>,
}

/// Capacities a player is connected with.
#[derive(Debug, Clone, Copy)]
pub struct Link {
    /// `Auth.features`.
    pub features: u64,
    /// The connection actor's inbox.
    pub inbox: usize,
    /// The outbound (writer) channel.
    pub outbound: usize,
    /// The input channel into the room.
    pub input: usize,
}

impl Default for Link {
    fn default() -> Self {
        Self {
            features: crate::wire::FEATURE_PUSH,
            inbox: 64,
            outbound: 64,
            input: 8,
        }
    }
}

/// The spike server.
pub struct Server<G: Game> {
    /// The router (the registry's table).
    pub router: Router,
    /// Rooms (one entry per shard).
    pub rooms: BTreeMap<RoomId, Vec<Room<G>>>,
    /// Their senders.
    pub handles: BTreeMap<RoomId, Vec<RoomHandles>>,
    /// Players.
    pub players: BTreeMap<Identity, Player>,
    next_conn: u64,
}

impl<G: Game> Server<G> {
    /// A server whose router mailbox holds `router_cap`.
    pub fn new(router_cap: usize) -> Self {
        Self {
            router: Router::new(router_cap),
            rooms: BTreeMap::new(),
            handles: BTreeMap::new(),
            players: BTreeMap::new(),
            next_conn: 1,
        }
    }

    /// A single room.
    pub fn room(&mut self, id: RoomId, game: G, caps: Caps) {
        let (r, h) = Room::new(id, game, caps);
        self.router.handle(RouterMsg::RegisterRoom {
            room: id,
            mailboxes: vec![h.events.clone()],
        });
        self.rooms.insert(id, vec![r]);
        self.handles.insert(id, vec![h]);
    }

    /// A sharded room.
    pub fn sharded(&mut self, id: RoomId, games: Vec<G>, caps: Caps) {
        let (rs, hs) = Room::sharded(id, games, caps, self.router.sender());
        self.router.handle(RouterMsg::RegisterRoom {
            room: id,
            mailboxes: hs.iter().map(|h| h.events.clone()).collect(),
        });
        self.rooms.insert(id, rs);
        self.handles.insert(id, hs);
    }

    /// A room (or shard).
    pub fn shard(&self, id: RoomId, i: u16) -> &Room<G> {
        &self.rooms[&id][i as usize]
    }

    /// A room (or shard), mutably.
    pub fn shard_mut(&mut self, id: RoomId, i: u16) -> &mut Room<G> {
        self.rooms.get_mut(&id).map(|v| &mut v[i as usize]).expect("room")
    }

    fn session(&mut self, name: &str, link: Link) -> (ConnLink, Player) {
        let conn = ConnId(self.next_conn);
        self.next_conn += 1;
        let (out, frames) = mpsc::channel(link.outbound);
        let (actor, inbox) = ConnActor::new(conn, link.inbox, out);
        (
            ConnLink {
                id: conn,
                inbox,
                features: link.features,
            },
            Player {
                id: Identity::new(name),
                conn,
                actor,
                frames,
                client: EventsClient::default(),
                input: None,
                features: link.features,
                where_: None,
            },
        )
    }

    /// AUTH succeeded (the registry's `Authed`).
    pub fn connect(&mut self, name: &str, link: Link) {
        let (cl, p) = self.session(name, link);
        self.router.handle(RouterMsg::Online {
            identity: p.id.clone(),
            conn: cl,
        });
        self.players.insert(p.id.clone(), p);
    }

    /// Join `room` (shard `shard`) with `carried` gold.
    pub fn join(&mut self, name: &str, room: RoomId, shard: u16, input_cap: usize, carried: u64) {
        let id = Identity::new(name);
        let (tx, rx) = mpsc::channel(input_cap);
        let p = self.players.get_mut(&id).expect("connected");
        p.input = Some(tx);
        p.where_ = Some((room, shard));
        let _ = self.handles[&room][shard as usize].control.try_send(Control::Join {
            identity: id.clone(),
            conn: p.conn,
            input: rx,
            carried,
        });
        self.router.handle(RouterMsg::Joined {
            identity: id,
            room,
            shard: Some(shard),
        });
    }

    /// Leave the room.
    pub fn leave(&mut self, name: &str) {
        let id = Identity::new(name);
        let p = self.players.get_mut(&id).expect("player");
        if let Some((room, shard)) = p.where_.take() {
            p.input = None;
            let _ = self.handles[&room][shard as usize].control.try_send(Control::Leave { identity: id.clone() });
            self.router.handle(RouterMsg::Left { identity: id, room });
        }
    }

    /// The transport died; the room parks the entity.
    pub fn park(&mut self, name: &str) {
        let id = Identity::new(name);
        let p = self.players.get_mut(&id).expect("player");
        p.input = None;
        if let Some((room, shard)) = p.where_ {
            let _ = self.handles[&room][shard as usize].control.try_send(Control::Park { identity: id.clone() });
        }
        self.router.handle(RouterMsg::Parked { identity: id });
    }

    /// A new session resumes the parked identity (the client half keeps
    /// its sequence state: the same app reconnecting).
    pub fn resume(&mut self, name: &str, link: Link, shard_now: u16) {
        let id = Identity::new(name);
        let old = self.players.remove(&id).expect("player");
        let (cl, mut p) = self.session(name, link);
        p.client = old.client;
        p.where_ = old.where_.map(|(r, _)| (r, shard_now));
        if let Some((room, shard)) = p.where_ {
            let (tx, rx) = mpsc::channel(link.input);
            p.input = Some(tx);
            let _ = self.handles[&room][shard as usize].control.try_send(Control::Resume {
                identity: id.clone(),
                conn: p.conn,
                input: rx,
            });
        }
        self.router.handle(RouterMsg::Resumed {
            identity: id.clone(),
            conn: cl,
        });
        self.players.insert(id, p);
    }

    /// The session ended for good.
    pub fn offline(&mut self, name: &str) {
        let id = Identity::new(name);
        if let Some(p) = self.players.remove(&id) {
            if let Some((room, shard)) = p.where_ {
                let _ = self.handles[&room][shard as usize].control.try_send(Control::Leave { identity: id.clone() });
            }
            self.router.handle(RouterMsg::Offline { identity: id, conn: p.conn });
        }
    }

    /// The harness's record of where a player is (after a migration).
    pub fn moved(&mut self, name: &str, room: RoomId, shard: u16) {
        if let Some(p) = self.players.get_mut(&Identity::new(name)) {
            p.where_ = Some((room, shard));
        }
    }

    /// The player's `Auth.features`.
    pub fn features(&self, name: &str) -> u64 {
        self.players[&Identity::new(name)].features
    }

    /// One server step: the router drains; every room/shard ticks; the
    /// router drains again (relocations); connections write; clients read.
    pub fn step(&mut self) {
        self.router.pump();
        for shards in self.rooms.values_mut() {
            for r in shards.iter_mut() {
                r.tick();
            }
        }
        self.router.pump();
        for p in self.players.values_mut() {
            p.actor.pump();
            while let Ok(f) = p.frames.try_recv() {
                p.client.ingest(&f);
            }
        }
    }

    /// A player's client half.
    pub fn client(&mut self, name: &str) -> &mut EventsClient {
        &mut self.players.get_mut(&Identity::new(name)).expect("player").client
    }
}

/// The worked examples' room logic: wallets fed by GoldReceived and
/// BalanceChanged (WORLD legs), confirmed balances (RESPONSES), purchases
/// (READ). No ECS — `Owner` is the game's own key (docs/EVENTS.md §8
/// shows the bevy_ecs and hecs forms).
#[derive(Default)]
pub struct Wallets {
    /// owner → (who, gold, ledger version).
    pub wallets: BTreeMap<Owner, (Identity, u64, u64)>,
    /// Every event taken: (who, publisher, seq) — the order proof.
    pub seen: Vec<(String, u32, u64)>,
    /// Purchases made: (who, amount).
    pub bought: Vec<(String, u64)>,
    /// Purchases refused (not enough gold).
    pub refused: u64,
    /// Room-wide BalanceChanged reasons (push_room).
    pub notes: Vec<String>,
    /// When false the game takes nothing (untaken counting).
    pub deaf: bool,
}

impl Game for Wallets {
    fn boot(&mut self, events: &mut Events) {
        events.accept::<GoldReceived>().expect("boot");
        events.accept::<BalanceChanged>().expect("boot");
    }

    fn join(&mut self, owner: Owner, who: &Identity, carried: u64) {
        self.wallets.insert(owner, (who.clone(), carried, 0));
    }

    fn leave(&mut self, owner: Owner) -> u64 {
        self.wallets.remove(&owner).map(|w| w.1).unwrap_or(0)
    }

    fn update(&mut self, io: Io<'_>) {
        if !self.deaf {
            // Events first, then input: a purchase this tick sees gold that
            // arrived this tick (EVENTS ran before READ).
            for e in io.events.take::<GoldReceived>() {
                if let Some(w) = self.wallets.get_mut(&e.owner) {
                    w.1 += e.body.amount;
                    w.2 = w.2.max(e.body.version);
                    self.seen.push((w.0.as_str().to_owned(), e.from.0, e.seq));
                }
            }
            for e in io.events.take::<BalanceChanged>() {
                match &e.to {
                    Addressee::Room => self.notes.push(e.body.reason.clone()),
                    Addressee::Player(_) => {
                        if let Some(w) = self.wallets.get_mut(&e.owner)
                            && e.body.version > w.2
                        {
                            w.1 = e.body.balance;
                            w.2 = e.body.version;
                            self.seen.push((w.0.as_str().to_owned(), e.from.0, e.seq));
                        }
                    }
                }
            }
        }
        for c in io.responses {
            if let Some(w) = self.wallets.get_mut(&c.owner) {
                w.1 = c.value;
            }
        }
        for (owner, a) in io.actions {
            if a.op != 1 {
                continue;
            }
            if let Some(w) = self.wallets.get_mut(owner) {
                if w.1 >= a.amount {
                    w.1 -= a.amount;
                    self.bought.push((w.0.as_str().to_owned(), a.amount));
                } else {
                    self.refused += 1;
                }
            }
        }
    }

    fn hash(&self, h: &mut Fnv) {
        for (o, (who, gold, ver)) in &self.wallets {
            h.u64(o.0);
            h.bytes(who.as_str().as_bytes());
            h.u64(*gold);
            h.u64(*ver);
        }
        h.u64(self.seen.len() as u64);
        h.u64(self.bought.len() as u64);
        h.u64(self.refused);
    }
}

/// The wallet of `who` in a room's game.
pub fn gold(w: &Wallets, who: &str) -> Option<u64> {
    w.wallets.values().find(|(id, _, _)| id.as_str() == who).map(|w| w.1)
}
