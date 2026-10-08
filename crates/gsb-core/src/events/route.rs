//! The routing table (`docs/EVENTS.md` §6): every recipient of every
//! push, every leg, numbered and delivered — or counted under the name of
//! what stopped it.
//!
//! It runs inside the registry actor (decision 3: the facts it needs —
//! who is authenticated, in which room, parked or live — are the
//! registry's own tables), but it is a struct of its own (§20 E2) that
//! reads those facts through the narrow [`Locate`] trait and keeps only
//! what is routing's: the sequence numbers and the counters. Moving it to
//! a dedicated router actor (or K of them, by identity hash) when the
//! loadgen measurement asks for it (§12) means implementing [`Locate`]
//! over facts the registry forwards — the routing code below is
//! unchanged.
//!
//! **Sequence numbers.** Per (publisher, identity, leg), from 1, spent
//! when the leg is ROUTED: a leg lost further on (a parked session, an
//! old client, a full room) leaves a gap its receiver sees. CLIENT and
//! WORLD count apart (§18 finding 3: a client never sees WORLD legs and
//! must not read them as gaps). The numbers live while the identity has a
//! row in the registry — from its first authenticated session until it
//! is offline and not parked; a resume keeps them, a new life starts a
//! new `stream` (the PUSH frame's, §20 E5) from 1. Room-wide WORLD legs
//! are numbered per (publisher, room).
//!
//! **Order of the legs (§20 E11).** A BOTH push routes its WORLD leg
//! first (the room usually sees it before the client does) — not a
//! guarantee: the two legs are independent.

use std::collections::HashMap;

use crate::calls::Identity;
use crate::channel::{Mailbox, TrySend};
use crate::conn::ConnIn;
use crate::events::{Addressee, EventIn, PublisherId, Push, Target, WorldPush};
use crate::id::RoomId;
use crate::metrics::RouterCounts;

/// A recipient's live session: where its CLIENT leg goes.
pub(crate) struct Live<'a> {
    /// Its connection actor's inbox.
    pub(crate) inbox: &'a Mailbox<ConnIn>,
    /// `Auth.features` the client announced.
    pub(crate) features: u64,
}

/// Where a recipient's WORLD leg goes.
pub(crate) enum Place<'a> {
    /// In no room (before its first join, between rooms).
    Nowhere,
    /// In a room the router can reach: that actor's (the room's, or the
    /// member's shard's) event mailbox — `None` when the room's entry or
    /// the shard is not known (it ended).
    In(Option<&'a Mailbox<EventIn>>),
}

/// One recipient as the registry's tables know it.
pub(crate) struct Located<'a> {
    /// Its live session (`None`: parked — the transport died, the entity
    /// waits in its room).
    pub(crate) live: Option<Live<'a>>,
    /// Its place.
    pub(crate) place: Place<'a>,
}

/// The facts routing reads (the registry implements it over its
/// connection and room tables).
pub(crate) trait Locate {
    /// `identity`'s row, or `None`: offline (no authenticated session,
    /// nothing parked).
    fn locate(&self, identity: &str) -> Option<Located<'_>>;
    /// The identities with a membership in `room` (`push_room`'s CLIENT
    /// leg), each once.
    fn members(&self, room: RoomId) -> Vec<String>;
    /// Every identity with a row (`push_all`'s CLIENT leg), each once.
    fn identities(&self) -> Vec<String>;
    /// `room`'s event mailboxes, one per actor (a single room: one; a
    /// sharded room: one per shard); `None` for a room the registry does
    /// not hold.
    fn doors(&self, room: RoomId) -> Option<Vec<&Mailbox<EventIn>>>;
    /// Every room the registry holds (`push_all`'s WORLD leg).
    fn rooms(&self) -> Vec<RoomId>;
}

/// The two legs of a push.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Leg {
    Client,
    World,
}

/// An identity's numbers (its row's routing state).
#[derive(Debug)]
struct Numbers {
    stream: u64,
    /// Next number per (publisher, leg) — a handful of entries.
    seqs: Vec<(PublisherId, Leg, u64)>,
}

impl Numbers {
    fn stamp(&mut self, p: PublisherId, leg: Leg) -> u64 {
        match self.seqs.iter_mut().find(|s| s.0 == p && s.1 == leg) {
            Some(s) => {
                s.2 += 1;
                s.2
            }
            None => {
                self.seqs.push((p, leg, 1));
                1
            }
        }
    }
}

/// The routing table: numbers and counters (see the module docs).
#[derive(Debug, Default)]
pub(crate) struct EventRoutes {
    numbers: HashMap<String, Numbers>,
    rooms: HashMap<RoomId, Vec<(PublisherId, u64)>>,
    /// The last stream minted (streams start at 1: a sent push never
    /// carries 0).
    streams: u64,
    counts: RouterCounts,
}

impl EventRoutes {
    /// The counters (cumulative).
    pub(crate) fn counts(&self) -> RouterCounts {
        self.counts
    }

    /// A push left in the router's mailbox at its stop.
    pub(crate) fn count_unread(&mut self) {
        self.counts.events_unread += 1;
    }

    /// `identity` has no row any more (offline, not parked): its numbers
    /// end; a later session starts a new stream from 1.
    pub(crate) fn forget(&mut self, identity: &str) {
        self.numbers.remove(identity);
    }

    /// `room` ended: its room-wide numbers end (a rebuilt room starts
    /// from 1).
    pub(crate) fn room_gone(&mut self, room: RoomId) {
        self.rooms.remove(&room);
    }

    /// Route one push.
    pub(crate) fn route(&mut self, p: Push, at: &dyn Locate) {
        self.counts.events_routed += 1;
        match &p.to {
            Target::One(id) => self.recipient(&p, id, at),
            Target::Many(ids) => {
                for id in ids.iter() {
                    self.recipient(&p, id, at);
                }
            }
            Target::Room(room) => {
                let room = *room;
                if p.dest.world() {
                    self.room_wide(&p, room, at);
                }
                if p.dest.client() {
                    for id in at.members(room) {
                        self.client_leg(&p, &id, at);
                    }
                }
            }
            Target::All => {
                if p.dest.world() {
                    for room in at.rooms() {
                        self.room_wide(&p, room, at);
                    }
                }
                if p.dest.client() {
                    for id in at.identities() {
                        self.client_leg(&p, &id, at);
                    }
                }
            }
        }
    }

    fn numbers(&mut self, identity: &str) -> &mut Numbers {
        if !self.numbers.contains_key(identity) {
            self.streams += 1;
            self.numbers.insert(
                identity.to_owned(),
                Numbers {
                    stream: self.streams,
                    seqs: Vec::new(),
                },
            );
        }
        self.numbers.get_mut(identity).expect("inserted above")
    }

    /// One identity-addressed recipient: `offline` once, or each of its
    /// legs (WORLD first, §20 E11).
    fn recipient(&mut self, p: &Push, id: &Identity, at: &dyn Locate) {
        if id.as_str().is_empty() || at.locate(id.as_str()).is_none() {
            self.counts.events_offline += 1;
            return;
        }
        if p.dest.world() {
            self.world_leg(p, id, at);
        }
        if p.dest.client() {
            self.client_leg(p, id.as_str(), at);
        }
    }

    fn client_leg(&mut self, p: &Push, id: &str, at: &dyn Locate) {
        let Some(row) = at.locate(id) else {
            self.counts.events_offline += 1;
            return;
        };
        let n = self.numbers(id);
        let stream = n.stream;
        let seq = n.stamp(p.publisher, Leg::Client);
        let Some(live) = row.live else {
            self.counts.events_client_parked += 1;
            return;
        };
        if live.features & gsb_protocol::FEATURE_PUSH == 0 {
            self.counts.events_client_unsupported += 1;
            return;
        }
        let msg = ConnIn::Push {
            publisher: p.publisher,
            stream,
            seq,
            op: p.op,
            body: p.body.clone(),
        };
        match crate::channel::try_send(live.inbox, msg) {
            TrySend::Sent => self.counts.events_client_sent += 1,
            TrySend::Full => self.counts.events_client_refused_conn_full += 1,
            TrySend::Closed => self.counts.events_client_refused_conn_gone += 1,
        }
    }

    fn world_leg(&mut self, p: &Push, id: &Identity, at: &dyn Locate) {
        let Some(row) = at.locate(id.as_str()) else {
            return;
        };
        let seq = self.numbers(id.as_str()).stamp(p.publisher, Leg::World);
        let Place::In(door) = row.place else {
            self.counts.events_world_no_room += 1;
            return;
        };
        let leg = WorldPush {
            to: Addressee::Player(id.clone()),
            publisher: p.publisher,
            seq,
            op: p.op,
            body: p.body.clone(),
        };
        self.send(door, leg);
    }

    /// A room-wide WORLD leg: every actor of the room once (§20 E9), one
    /// number per (publisher, room).
    fn room_wide(&mut self, p: &Push, room: RoomId, at: &dyn Locate) {
        let Some(doors) = at.doors(room) else {
            self.counts.events_world_no_room += 1;
            return;
        };
        let seqs = self.rooms.entry(room).or_default();
        let seq = match seqs.iter_mut().find(|s| s.0 == p.publisher) {
            Some(s) => {
                s.1 += 1;
                s.1
            }
            None => {
                seqs.push((p.publisher, 1));
                1
            }
        };
        for door in doors {
            let leg = WorldPush {
                to: Addressee::Room,
                publisher: p.publisher,
                seq,
                op: p.op,
                body: p.body.clone(),
            };
            self.send(Some(door), leg);
        }
    }

    fn send(&mut self, door: Option<&Mailbox<EventIn>>, leg: WorldPush) {
        let Some(door) = door else {
            self.counts.events_world_refused_room_gone += 1;
            return;
        };
        match crate::channel::try_send(door, EventIn::Push(leg)) {
            TrySend::Sent => self.counts.events_world_sent += 1,
            TrySend::Full => self.counts.events_world_refused_room_full += 1,
            TrySend::Closed => self.counts.events_world_refused_room_gone += 1,
        }
    }
}
