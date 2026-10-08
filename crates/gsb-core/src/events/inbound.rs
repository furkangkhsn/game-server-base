//! The EVENTS phase both tick actors run — written ONCE (the
//! `room/requests.rs` discipline): a room and a shard each own an
//! [`Inbound`] (their own bounded event mailbox + their [`Events`]
//! table) and call [`Inbound::drain`] between RESPONSES and READ.
//!
//! ```text
//! CONTROL → RESPONSES → EVENTS → READ → CONVERT → SYSTEMS → FLUSH → BROADCAST
//!                        drain()                                     end_tick()
//! ```
//!
//! The mailbox is the ACTOR's, not a session's: a client flooding its
//! own input channel cannot crowd out its events (§3.1), and a parked
//! member (no session) still gets its WORLD legs (§20 E1).

use std::collections::HashMap;

use bytes::Bytes;

use crate::calls::{Calls, Owner};
use crate::channel::{Inbox, Mailbox, channel};
use crate::events::fence::{Fences, Way};
use crate::events::{Addressee, Events, PublisherId};
use crate::id::PlayerId;
use crate::metrics::EventCounts;
use crate::room::RoomConn;

/// One WORLD leg on its way to a room or shard.
#[derive(Debug)]
pub(crate) struct WorldPush {
    pub(crate) to: Addressee,
    pub(crate) publisher: PublisherId,
    pub(crate) seq: u64,
    pub(crate) op: u16,
    pub(crate) body: Bytes,
}

/// What a room's (or shard's) event mailbox carries: the WORLD legs and
/// the migration fence's seal, which rides the same FIFO behind every leg
/// the router sent before it (`docs/EVENTS.md` §7.3).
#[derive(Debug)]
pub(crate) enum EventIn {
    /// A WORLD leg.
    Push(WorldPush),
    /// The router moved `identity` away from this shard (its migration
    /// `mig`): nothing more for it comes here — the shard answers with
    /// its fence once what it still had is handed on.
    Seal { identity: String, mig: u64 },
}

/// A WORLD leg one shard hands another for a member that migrated
/// (`ShardMsg::Forward`, `docs/EVENTS.md` §7.3): opaque outside the core.
#[derive(Debug)]
pub struct Forwarded(pub(crate) WorldPush);

/// An actor's event inbox and table.
pub(crate) struct Inbound {
    /// The sender the registry routes into (cloned out by
    /// [`Self::mailbox`]). The actor keeps one: the mailbox closes when
    /// the actor ends (stops, or dies with its task), and the router then
    /// counts `events_world_refused_room_gone`.
    tx: Mailbox<EventIn>,
    rx: Inbox<EventIn>,
    capacity: usize,
    /// The table the game takes from.
    pub(crate) table: Events,
    /// The migration fence (shards; a single room's stays empty).
    pub(crate) fences: Fences,
}

impl Inbound {
    /// A `capacity`-bounded mailbox over `table`.
    pub(crate) fn new(capacity: usize, table: Events) -> Self {
        let (tx, rx) = channel(capacity);
        Self {
            tx,
            rx,
            capacity: capacity.max(1),
            table,
            fences: Fences::default(),
        }
    }

    /// The mailbox the router sends this actor's WORLD legs to.
    pub(crate) fn mailbox(&self) -> Mailbox<EventIn> {
        self.tx.clone()
    }

    /// The table's counters.
    pub(crate) fn counts(&self) -> EventCounts {
        *self.table.counts()
    }

    /// EVENTS — every WORLD leg queued now (at most one mailbox's worth:
    /// a router still sending meanwhile cannot keep the tick here) enters
    /// the table, in arrival order: its addressee resolved to a member
    /// (`events_world_not_here` when the identity is no member of this
    /// actor), its owner resolved through the call binding — the
    /// session's explicit `calls.bind`, else `owner_of(player)`
    /// (`GameLogic::call_owner`; `None` → `events_unbound`) — then
    /// decoded as its accepted type (`events_no_handler`,
    /// `events_undecodable`). A parked member (detached row) is a member:
    /// its legs are delivered (§20 E1). On a shard the migration fence
    /// routes first (`fence.rs`, §7.3): the legs CONTROL let in go ahead
    /// of the mailbox; a leg for a member that crossed away is handed on
    /// or held, a seal answered with the fence.
    ///
    /// Cost when quiet: one empty `try_recv`, no allocation. The identity
    /// → member index is built once per tick that has a player-addressed
    /// leg (§18 finding 4), from the member table itself — it can never
    /// disagree with it.
    pub(crate) fn drain<G>(
        &mut self,
        conns: &HashMap<PlayerId, RoomConn<G>>,
        calls: &Calls,
        owner_of: &dyn Fn(PlayerId) -> Option<Owner>,
    ) {
        let mut members: Option<HashMap<&str, PlayerId>> = None;
        // The legs CONTROL let in (forwarded by a neighbour, released by
        // a fence) are older than the mailbox's: first.
        while let Some(p) = self.fences.ready.pop_front() {
            self.enter(p, true, &mut members, conns, calls, owner_of);
        }
        for _ in 0..self.capacity {
            let Ok(msg) = self.rx.try_recv() else {
                return;
            };
            match msg {
                EventIn::Push(p) => self.enter(p, false, &mut members, conns, calls, owner_of),
                EventIn::Seal { identity, mig } => {
                    self.fences.seal(&identity, mig, self.table.counts_mut());
                }
            }
        }
    }

    /// One leg: routed by the fence (`fence.rs`), then — when it is this
    /// actor's — owned and admitted.
    fn enter<'c, G>(
        &mut self,
        p: WorldPush,
        forwarded: bool,
        members: &mut Option<HashMap<&'c str, PlayerId>>,
        conns: &'c HashMap<PlayerId, RoomConn<G>>,
        calls: &Calls,
        owner_of: &dyn Fn(PlayerId) -> Option<Owner>,
    ) {
        let index = &mut *members;
        let member = |id: &str| {
            index
                .get_or_insert_with(|| member_index(conns))
                .contains_key(id)
        };
        let p = match self
            .fences
            .route(p, forwarded, member, self.table.counts_mut())
        {
            Way::Admit(p) => p,
            Way::NotHere => {
                self.table.counts_mut().events_world_not_here += 1;
                return;
            }
            Way::Kept => return,
        };
        let owner = match &p.to {
            Addressee::Room => Owner::ACTOR,
            Addressee::Player(identity) => {
                let index = members.get_or_insert_with(|| member_index(conns));
                let Some(&player) = index.get(identity.as_str()) else {
                    self.table.counts_mut().events_world_not_here += 1;
                    return;
                };
                let bound = conns
                    .get(&player)
                    .and_then(|rc| calls.bound(rc.conn))
                    .or_else(|| owner_of(player));
                let Some(owner) = bound else {
                    self.table.counts_mut().events_unbound += 1;
                    return;
                };
                owner
            }
        };
        self.table
            .admit(p.op, &p.body, p.to, owner, p.publisher, p.seq);
    }

    /// BROADCAST: what nobody took this tick is dropped and counted.
    pub(crate) fn end_tick(&mut self) {
        self.table.end_tick();
    }
}

/// identity → member. Anonymous members (empty identity) are never
/// addressed. Two rows of one identity (a session superseding another
/// mid-handover): the live one wins over a parked one.
fn member_index<G>(conns: &HashMap<PlayerId, RoomConn<G>>) -> HashMap<&str, PlayerId> {
    let mut index: HashMap<&str, PlayerId> = HashMap::with_capacity(conns.len());
    for (player, rc) in conns {
        if rc.identity.is_empty() {
            continue;
        }
        match index.get(rc.identity.as_str()) {
            Some(other) if !conns.get(other).is_some_and(|o| o.detached) || rc.detached => {}
            _ => {
                index.insert(rc.identity.as_str(), *player);
            }
        }
    }
    index
}
