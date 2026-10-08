//! The router (docs/EVENTS.md §6): the registry's identity → location
//! table, modelled as its own actor. In the engine (phase e1) this table
//! lives IN the registry actor, which already learns every fact it needs
//! (auth, join, leave, park, resume, close); `Relocated` is the one new
//! report (a shard that took a player in by migration). The table is a
//! plain struct, so moving it into a dedicated router actor — or N of
//! them, split by identity hash — when measurements ask for it changes no
//! routing code (§12, "measure, then split").
//!
//! One loop, one await (`recv`); every send is a `try_send` whose refusal
//! is counted under its own name.
//!
//! **Sequence numbers.** Per (publisher, identity, leg): the router
//! stamps each leg of each push for each identity, from 1, while the
//! identity has a row (from its first authenticated session until it is
//! offline and not parked). A leg that is then lost downstream still
//! consumed its number, so the receiver SEES the gap. CLIENT and WORLD
//! count separately — a client never sees WORLD legs, and must not read
//! them as gaps.

use std::collections::{BTreeMap, BTreeSet};

use bytes::Bytes;
use tokio::sync::mpsc::{self, error::TrySendError};

use crate::conn::ConnMsg;
use crate::contract::{ConnId, Identity, Leg, PublisherId, RoomId};
use crate::engine::{EngineEvent, Interest, Subscribers};
use crate::publisher::{Publisher, Push, Target};
use crate::room::{Addressee, Delivered, RoomEventMsg};
use crate::wire::FEATURE_PUSH;

/// A session as the router knows it (the registry's `ConnInfo.inbox` +
/// the features the connection reported at AUTH).
#[derive(Debug, Clone)]
pub struct ConnLink {
    /// The session.
    pub id: ConnId,
    /// The connection actor's inbox.
    pub inbox: mpsc::Sender<ConnMsg>,
    /// `Auth.features`.
    pub features: u64,
}

/// The router's mailbox vocabulary.
#[derive(Debug)]
pub enum RouterMsg {
    /// From a publisher.
    Push(Push),
    /// An authenticated session opened (`RegistryMsg::Authed`). A second
    /// session for an online identity supersedes the first (no event).
    Online { identity: Identity, conn: ConnLink },
    /// The session `conn` ended for good (`ConnClosed`, not parked).
    Offline { identity: Identity, conn: ConnId },
    /// The identity joined a room (`SpawnDone`; `shard`: the shard that
    /// took the join — the home pick, or the one whose ledger resumed it).
    Joined { identity: Identity, room: RoomId, shard: Option<u16> },
    /// The identity left its room (`LeaveDone`).
    Left { identity: Identity, room: RoomId },
    /// The transport died and the room parked the entity (`Detached`).
    Parked { identity: Identity },
    /// A parked identity resumed on a new session.
    Resumed { identity: Identity, conn: ConnLink },
    /// NEW: a shard took the identity in by migration (`mig` = the
    /// player's migration count, so a late report never wins).
    Relocated { identity: Identity, room: RoomId, shard: u16, mig: u64 },
    /// A room was created: its event mailboxes (one per shard).
    RegisterRoom { room: RoomId, mailboxes: Vec<mpsc::Sender<RoomEventMsg>> },
    /// A room ended.
    DropRoom { room: RoomId },
}

/// Where an identity's WORLD leg goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Place {
    /// In no room (between rooms, or before the first join).
    Nowhere,
    /// A single room, or shard `1` of room `0`.
    In(RoomId, u16),
}

#[derive(Debug)]
struct Row {
    session: Option<ConnLink>,
    parked: bool,
    place: Place,
    /// The highest migration count reported.
    mig: u64,
    /// Next sequence number per (publisher, leg) — a handful of entries.
    seqs: Vec<(PublisherId, Leg, u64)>,
}

impl Row {
    fn stamp(&mut self, p: PublisherId, leg: Leg) -> u64 {
        if let Some(s) = self.seqs.iter_mut().find(|s| s.0 == p && s.1 == leg) {
            s.2 += 1;
            s.2
        } else {
            self.seqs.push((p, leg, 1));
            1
        }
    }
}

/// Every outcome under its own name (docs/EVENTS.md §9).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct RouterCounts {
    /// Pushes taken from the mailbox.
    pub pushes: u64,
    /// Identity-addressed recipients with no row (offline).
    pub offline: u64,
    /// CLIENT legs handed to a connection actor.
    pub client_sent: u64,
    /// CLIENT legs for a parked identity (no session).
    pub client_parked: u64,
    /// CLIENT legs for a session without `FEATURE_PUSH`.
    pub client_unsupported: u64,
    /// CLIENT legs refused: connection inbox full.
    pub client_refused_conn_full: u64,
    /// CLIENT legs refused: connection gone.
    pub client_refused_conn_gone: u64,
    /// WORLD legs handed to a room/shard.
    pub world_sent: u64,
    /// WORLD legs for an identity in no room.
    pub world_no_room: u64,
    /// WORLD legs refused: the room's event mailbox full.
    pub world_refused_room_full: u64,
    /// WORLD legs refused: the room gone (or never registered).
    pub world_refused_room_gone: u64,
    /// Relocations applied / ignored as stale.
    pub relocations: u64,
    /// Stale relocations (a lower migration count than known).
    pub relocations_stale: u64,
    /// Seals sent to a migration's source shard (the FIFO fence, §7.3).
    pub seals_sent: u64,
    /// Seals refused (source mailbox full/gone: the target's hold times out).
    pub seals_refused: u64,
}

/// The router actor.
pub struct Router {
    rx: mpsc::Receiver<RouterMsg>,
    tx: mpsc::Sender<RouterMsg>,
    rows: BTreeMap<Identity, Row>,
    rooms: BTreeMap<RoomId, Vec<mpsc::Sender<RoomEventMsg>>>,
    /// Room → its members (push_room's CLIENT leg).
    members: BTreeMap<RoomId, BTreeSet<Identity>>,
    /// Room-addressed WORLD sequence numbers per (publisher, room).
    room_seqs: BTreeMap<(PublisherId, RoomId), u64>,
    next_publisher: u32,
    subscribers: Subscribers,
    counts: RouterCounts,
}

impl Router {
    /// A router with a bounded mailbox of `cap`.
    pub fn new(cap: usize) -> Self {
        let (tx, rx) = mpsc::channel(cap);
        Self {
            rx,
            tx,
            rows: BTreeMap::new(),
            rooms: BTreeMap::new(),
            members: BTreeMap::new(),
            room_seqs: BTreeMap::new(),
            next_publisher: 1,
            subscribers: Subscribers::default(),
            counts: RouterCounts::default(),
        }
    }

    /// The mailbox (the registry hands it to rooms, connections, …).
    pub fn sender(&self) -> mpsc::Sender<RouterMsg> {
        self.tx.clone()
    }

    /// Mint a publishing handle (composition root).
    pub fn publisher(&mut self) -> Publisher {
        let id = PublisherId(self.next_publisher);
        self.next_publisher += 1;
        Publisher::new(id, self.tx.clone())
    }

    /// Subscribe to engine events (composition root; §5.4). A bounded
    /// receiver: a full subscriber loses the event, counted per name.
    pub fn subscribe(&mut self, name: &'static str, interest: Interest, cap: usize) -> mpsc::Receiver<EngineEvent> {
        self.subscribers.add(name, interest, cap)
    }

    /// The counters.
    pub fn counts(&self) -> &RouterCounts {
        &self.counts
    }

    /// The engine-event subscribers' counters.
    pub fn subscribers(&self) -> &Subscribers {
        &self.subscribers
    }

    /// Where an identity's WORLD leg goes now (tests).
    pub fn place(&self, who: &Identity) -> Option<Place> {
        self.rows.get(who).map(|r| r.place)
    }

    /// Drain what is queued now (the synchronous tests' "the router ran").
    pub fn pump(&mut self) -> usize {
        let mut n = 0;
        while let Ok(m) = self.rx.try_recv() {
            self.handle(m);
            n += 1;
        }
        n
    }

    /// The actor loop: ONE await per iteration. Returns itself when every
    /// sender is gone (tests read the counters).
    pub async fn run(mut self) -> Self {
        // `self.tx` would keep the channel open forever: drop our copy.
        let (dead, _) = mpsc::channel(1);
        self.tx = dead;
        while let Some(m) = self.rx.recv().await {
            self.handle(m);
        }
        self
    }

    /// One message.
    pub fn handle(&mut self, m: RouterMsg) {
        match m {
            RouterMsg::Push(p) => self.push(p),
            RouterMsg::Online { identity, conn } => {
                let fresh = !self.rows.contains_key(&identity);
                let row = self.rows.entry(identity.clone()).or_insert(Row {
                    session: None,
                    parked: false,
                    place: Place::Nowhere,
                    mig: 0,
                    seqs: Vec::new(),
                });
                row.session = Some(conn);
                if fresh {
                    self.subscribers.emit(EngineEvent::Online { identity });
                }
            }
            RouterMsg::Offline { identity, conn } => {
                let current = self.rows.get(&identity).and_then(|r| r.session.as_ref()).map(|s| s.id);
                // A superseded session's close is not the identity's.
                if current == Some(conn) {
                    if let Some(row) = self.rows.remove(&identity)
                        && let Place::In(room, _) = row.place
                    {
                        self.leave_members(room, &identity);
                        self.subscribers.emit(EngineEvent::Left { identity: identity.clone(), room });
                    }
                    self.subscribers.emit(EngineEvent::Offline { identity });
                }
            }
            RouterMsg::Joined { identity, room, shard } => {
                if let Some(row) = self.rows.get_mut(&identity) {
                    row.place = Place::In(room, shard.unwrap_or(0));
                    self.members.entry(room).or_default().insert(identity.clone());
                    self.subscribers.emit(EngineEvent::Joined { identity, room });
                }
            }
            RouterMsg::Left { identity, room } => {
                if let Some(row) = self.rows.get_mut(&identity)
                    && matches!(row.place, Place::In(r, _) if r == room)
                {
                    row.place = Place::Nowhere;
                    self.leave_members(room, &identity);
                    self.subscribers.emit(EngineEvent::Left { identity, room });
                }
            }
            RouterMsg::Parked { identity } => {
                if let Some(row) = self.rows.get_mut(&identity) {
                    row.session = None;
                    row.parked = true;
                    let room = match row.place {
                        Place::In(r, _) => Some(r),
                        Place::Nowhere => None,
                    };
                    self.subscribers.emit(EngineEvent::Parked { identity, room });
                }
            }
            RouterMsg::Resumed { identity, conn } => {
                if let Some(row) = self.rows.get_mut(&identity) {
                    row.session = Some(conn);
                    row.parked = false;
                    let room = match row.place {
                        Place::In(r, _) => Some(r),
                        Place::Nowhere => None,
                    };
                    self.subscribers.emit(EngineEvent::Resumed { identity, room });
                }
            }
            RouterMsg::Relocated { identity, room, shard, mig } => self.relocate(identity, room, shard, mig),
            RouterMsg::RegisterRoom { room, mailboxes } => {
                self.rooms.insert(room, mailboxes);
            }
            RouterMsg::DropRoom { room } => {
                self.rooms.remove(&room);
            }
        }
    }

    fn leave_members(&mut self, room: RoomId, who: &Identity) {
        if let Some(m) = self.members.get_mut(&room) {
            m.remove(who);
            if m.is_empty() {
                self.members.remove(&room);
            }
        }
    }

    /// The FIFO fence (§7.3): point the identity at its new shard, and
    /// SEAL the old one — the seal rides the old shard's event mailbox
    /// BEHIND every event already sent there, and comes back to the new
    /// shard (as a fence, on the shard-to-shard FIFO) behind every event
    /// the old shard forwards. The new shard holds direct events until
    /// the fence: per (publisher, identity) order survives the move.
    fn relocate(&mut self, identity: Identity, room: RoomId, shard: u16, mig: u64) {
        let Some(row) = self.rows.get_mut(&identity) else {
            return;
        };
        if mig <= row.mig {
            self.counts.relocations_stale += 1;
            return;
        }
        let old = row.place;
        row.place = Place::In(room, shard);
        row.mig = mig;
        self.counts.relocations += 1;
        if let Place::In(r, old_shard) = old
            && r == room
            && old_shard != shard
        {
            let seal = RoomEventMsg::Seal { identity, mig };
            match self.rooms.get(&room).and_then(|m| m.get(old_shard as usize)).map(|tx| tx.try_send(seal)) {
                Some(Ok(())) => self.counts.seals_sent += 1,
                _ => self.counts.seals_refused += 1,
            }
        }
    }

    fn push(&mut self, p: Push) {
        self.counts.pushes += 1;
        match &p.to {
            Target::One(id) => self.route_identity(&p, id),
            Target::Many(ids) => {
                for id in ids.iter() {
                    self.route_identity(&p, id);
                }
            }
            Target::Room(room) => {
                let room = *room;
                if p.dest.client() {
                    let who: Vec<Identity> = self.members.get(&room).map(|m| m.iter().cloned().collect()).unwrap_or_default();
                    for id in &who {
                        self.client_leg(&p, id);
                    }
                }
                if p.dest.world() {
                    self.room_world(&p, room);
                }
            }
            Target::All => {
                if p.dest.client() {
                    let who: Vec<Identity> = self.rows.keys().cloned().collect();
                    for id in &who {
                        self.client_leg(&p, id);
                    }
                }
                if p.dest.world() {
                    let rooms: Vec<RoomId> = self.rooms.keys().copied().collect();
                    for room in rooms {
                        self.room_world(&p, room);
                    }
                }
            }
        }
    }

    fn route_identity(&mut self, p: &Push, id: &Identity) {
        if !self.rows.contains_key(id) {
            // Counted once per recipient, whatever its legs.
            self.counts.offline += 1;
            return;
        }
        if p.dest.client() {
            self.client_leg(p, id);
        }
        if p.dest.world() {
            self.world_leg(p, id);
        }
    }

    fn client_leg(&mut self, p: &Push, id: &Identity) {
        let Some(row) = self.rows.get_mut(id) else {
            self.counts.offline += 1;
            return;
        };
        let seq = row.stamp(p.publisher, Leg::Client);
        let Some(s) = &row.session else {
            self.counts.client_parked += 1;
            return;
        };
        if s.features & FEATURE_PUSH == 0 {
            self.counts.client_unsupported += 1;
            return;
        }
        let msg = ConnMsg::Push {
            publisher: p.publisher,
            seq,
            op: p.op,
            body: p.body.clone(),
        };
        match s.inbox.try_send(msg) {
            Ok(()) => self.counts.client_sent += 1,
            Err(TrySendError::Full(_)) => self.counts.client_refused_conn_full += 1,
            Err(TrySendError::Closed(_)) => self.counts.client_refused_conn_gone += 1,
        }
    }

    fn world_leg(&mut self, p: &Push, id: &Identity) {
        let Some(row) = self.rows.get_mut(id) else {
            return;
        };
        let seq = row.stamp(p.publisher, Leg::World);
        let Place::In(room, shard) = row.place else {
            self.counts.world_no_room += 1;
            return;
        };
        let d = Delivered {
            to: Addressee::Player(id.clone()),
            publisher: p.publisher,
            seq,
            op: p.op,
            body: p.body.clone(),
        };
        self.send_room(room, shard, d);
    }

    fn room_world(&mut self, p: &Push, room: RoomId) {
        let seq = {
            let s = self.room_seqs.entry((p.publisher, room)).or_insert(0);
            *s += 1;
            *s
        };
        let shards = self.rooms.get(&room).map(|m| m.len()).unwrap_or(0);
        if shards == 0 {
            self.counts.world_refused_room_gone += 1;
        }
        for i in 0..shards {
            let d = Delivered {
                to: Addressee::Room,
                publisher: p.publisher,
                seq,
                op: p.op,
                body: p.body.clone(),
            };
            self.send_room(room, i as u16, d);
        }
    }

    fn send_room(&mut self, room: RoomId, shard: u16, d: Delivered) {
        let Some(tx) = self.rooms.get(&room).and_then(|m| m.get(shard as usize)) else {
            self.counts.world_refused_room_gone += 1;
            return;
        };
        match tx.try_send(RoomEventMsg::Event(d)) {
            Ok(()) => self.counts.world_sent += 1,
            Err(TrySendError::Full(_)) => self.counts.world_refused_room_full += 1,
            Err(TrySendError::Closed(_)) => self.counts.world_refused_room_gone += 1,
        }
    }
}

/// The body a test can compare by pointer (shared, not copied).
pub fn body_ptr(b: &Bytes) -> usize {
    b.as_ptr() as usize
}
