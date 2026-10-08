//! A room (or shard) actor reduced to what events touch (docs/EVENTS.md
//! §3, §7): its OWN bounded event mailbox — never the members' input
//! channels — drained in the EVENTS phase, between RESPONSES and READ:
//!
//! ```text
//! CONTROL → RESPONSES → EVENTS → READ → CONVERT → SYSTEMS → FLUSH → BROADCAST
//! ```
//!
//! For a shard, the EVENTS phase also keeps WORLD legs correct across a
//! migration: the source keeps a TOMBSTONE (identity → target) and
//! forwards stragglers on the shard-to-shard FIFO (`Control::Forward`,
//! behind the `MigrateIn`); the target holds the router's DIRECT events
//! for that identity until the FENCE arrives on the same FIFO behind the
//! last forwarded one (§7.3).
//!
//! Synchronous: every receive is a `try_recv`, every send a `try_send`.

use std::collections::BTreeMap;

use bytes::Bytes;
use tokio::sync::mpsc::{self, error::TrySendError};

use crate::contract::{ConnId, Identity, Owner, PublisherId, RoomId};
use crate::events::Events;
use crate::record::{ControlRec, Fnv, Input, TickRecord};
use crate::router::RouterMsg;

/// Who a delivered event is for, inside the room.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Addressee {
    /// One player (its owner is resolved at EVENTS).
    Player(Identity),
    /// The room as a whole (`push_room`, `push_all`): owner `ACTOR`.
    Room,
}

/// One WORLD leg as the room receives it.
#[derive(Debug, Clone)]
pub struct Delivered {
    /// The addressee.
    pub to: Addressee,
    /// The publisher.
    pub publisher: PublisherId,
    /// The WORLD-leg sequence number.
    pub seq: u64,
    /// The op.
    pub op: u16,
    /// The shared body.
    pub body: Bytes,
}

/// The room's event mailbox vocabulary.
#[derive(Debug, Clone)]
pub enum RoomEventMsg {
    /// A WORLD leg.
    Event(Delivered),
    /// The router moved `identity` away from this shard (migration
    /// `mig`): nothing more for it will come here.
    Seal { identity: Identity, mig: u64 },
}

/// A client input (READ's model; the engine's actions and requests).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Action {
    /// What (1 = buy).
    pub op: u16,
    /// For how much.
    pub amount: u64,
}

/// A call result (RESPONSES' model).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Completion {
    /// Whose.
    pub owner: Owner,
    /// The value (e.g. a confirmed balance).
    pub value: u64,
}

/// The control channel (CONTROL) and, for shards, the shard protocol FIFO.
#[derive(Debug)]
pub enum Control {
    /// A join (the engine's control `Join`).
    Join { identity: Identity, conn: ConnId, input: mpsc::Receiver<Action>, carried: u64 },
    /// A leave.
    Leave { identity: Identity },
    /// A park: the transport died, the entity stays.
    Park { identity: Identity },
    /// A resume on a new session.
    Resume { identity: Identity, conn: ConnId, input: mpsc::Receiver<Action> },
    /// Shard protocol: a player crosses in (the engine's `Migrate`).
    MigrateIn { identity: Identity, conn: ConnId, input: Option<mpsc::Receiver<Action>>, mig: u64, carried: u64 },
    /// Shard protocol: a WORLD leg the source received after the player left.
    Forward(Delivered),
    /// Shard protocol: the source will forward nothing more for `identity`.
    Fence { identity: Identity, mig: u64 },
}

/// What the game sees this tick.
pub struct Io<'a> {
    /// The step.
    pub tick: u64,
    /// The event table.
    pub events: &'a mut Events,
    /// RESPONSES' results.
    pub responses: &'a [Completion],
    /// READ's inputs, with their owners.
    pub actions: &'a [(Owner, Action)],
}

/// The room logic (the engine's `GameLogic`, reduced).
pub trait Game {
    /// Boot: accept event types.
    fn boot(&mut self, events: &mut crate::events::Events);
    /// A player arrives (join or migrate-in) with its carried state.
    fn join(&mut self, owner: Owner, who: &Identity, carried: u64);
    /// A player leaves (leave or migrate-out); returns its carried state.
    fn leave(&mut self, owner: Owner) -> u64;
    /// SYSTEMS.
    fn update(&mut self, io: Io<'_>);
    /// The state hash.
    fn hash(&self, h: &mut Fnv);
}

/// The room's routing counters (the table counts the rest).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct RoomCounts {
    /// A player-addressed leg for an identity not here (left, expired
    /// tombstone): `events_world_not_here`.
    pub not_here: u64,
    /// Forwarded to a migration's target.
    pub forwarded: u64,
    /// Forward refused (target's FIFO full/gone).
    pub forward_refused: u64,
    /// Held behind a fence.
    pub fence_held: u64,
    /// Released by a fence.
    pub fence_released: u64,
    /// Dropped: a fence's hold was full.
    pub fence_overflow: u64,
    /// A fence that never came (held events released by the timeout).
    pub fence_timeouts: u64,
    /// A seal or fence nobody waited for.
    pub fence_lost: u64,
    /// `Relocated` refused by the router's mailbox.
    pub relocations_refused: u64,
    /// Tombstones that expired.
    pub tombstones_expired: u64,
    /// Client inputs read.
    pub inputs_read: u64,
}

/// Ticks a tombstone forwards stragglers.
pub const TOMB_TTL: u64 = 64;
/// Ticks a fence may hold before releasing anyway.
pub const FENCE_TTL: u64 = 64;
/// Held events per fenced identity.
pub const HOLD_CAP: usize = 256;
/// Inputs READ takes per member per tick.
pub const READ_BUDGET: usize = 16;

struct Member {
    owner: Owner,
    conn: ConnId,
    input: Option<mpsc::Receiver<Action>>,
}

struct Tomb {
    to: u16,
    mig_out: u64,
    until: u64,
    /// An incoming fence this shard still waited for when the player left
    /// again: the outgoing fence waits for it (chained migrations).
    awaiting_in: Option<u64>,
    /// The router's seal for `mig_out` arrived.
    sealed: bool,
}

struct Hold {
    mig: u64,
    since: u64,
    held: Vec<Delivered>,
}

struct ShardLink {
    index: u16,
    neighbors: Vec<mpsc::Sender<Control>>,
    tombs: BTreeMap<Identity, Tomb>,
    holds: BTreeMap<Identity, Hold>,
    migs: BTreeMap<Identity, u64>,
}

/// The senders a room is reached by.
#[derive(Debug, Clone)]
pub struct RoomHandles {
    /// CONTROL.
    pub control: mpsc::Sender<Control>,
    /// The room's OWN event mailbox (the router's WORLD legs).
    pub events: mpsc::Sender<RoomEventMsg>,
    /// RESPONSES.
    pub completions: mpsc::Sender<Completion>,
}

/// Channel capacities.
#[derive(Debug, Clone, Copy)]
pub struct Caps {
    /// Control.
    pub control: usize,
    /// The event mailbox (`RoomConfig::event_capacity`).
    pub events: usize,
    /// Completions.
    pub completions: usize,
}

impl Default for Caps {
    fn default() -> Self {
        Self {
            control: 256,
            events: 256,
            completions: 256,
        }
    }
}

/// A room or shard actor.
pub struct Room<G: Game> {
    /// The room.
    pub id: RoomId,
    shard: Option<ShardLink>,
    control_rx: mpsc::Receiver<Control>,
    events_rx: mpsc::Receiver<RoomEventMsg>,
    completions_rx: mpsc::Receiver<Completion>,
    router: Option<mpsc::Sender<RouterMsg>>,
    members: BTreeMap<Identity, Member>,
    next_owner: u64,
    events: Events,
    game: G,
    tick: u64,
    counts: RoomCounts,
    record: Option<Vec<TickRecord>>,
}

impl<G: Game> Room<G> {
    /// A single room.
    pub fn new(id: RoomId, mut game: G, caps: Caps) -> (Self, RoomHandles) {
        let (control, control_rx) = mpsc::channel(caps.control);
        let (events, events_rx) = mpsc::channel(caps.events);
        let (completions, completions_rx) = mpsc::channel(caps.completions);
        let mut table = Events::default();
        game.boot(&mut table);
        (
            Self {
                id,
                shard: None,
                control_rx,
                events_rx,
                completions_rx,
                router: None,
                members: BTreeMap::new(),
                next_owner: 1,
                events: table,
                game,
                tick: 0,
                counts: RoomCounts::default(),
                record: None,
            },
            RoomHandles {
                control,
                events,
                completions,
            },
        )
    }

    /// N shards of one room, wired to each other (the shard FIFOs).
    pub fn sharded(id: RoomId, games: Vec<G>, caps: Caps, router: mpsc::Sender<RouterMsg>) -> (Vec<Self>, Vec<RoomHandles>) {
        let mut rooms = Vec::new();
        let mut handles = Vec::new();
        for g in games {
            let (r, h) = Self::new(id, g, caps);
            rooms.push(r);
            handles.push(h);
        }
        let neighbors: Vec<_> = handles.iter().map(|h| h.control.clone()).collect();
        for (i, r) in rooms.iter_mut().enumerate() {
            r.next_owner = (i as u64) * 1_000_000 + 1;
            r.router = Some(router.clone());
            r.shard = Some(ShardLink {
                index: i as u16,
                neighbors: neighbors.clone(),
                tombs: BTreeMap::new(),
                holds: BTreeMap::new(),
                migs: BTreeMap::new(),
            });
        }
        (rooms, handles)
    }

    /// Record every step (hashes included).
    pub fn record(&mut self) {
        self.record = Some(Vec::new());
    }

    /// The record so far.
    pub fn take_record(&mut self) -> Vec<TickRecord> {
        self.record.as_mut().map(std::mem::take).unwrap_or_default()
    }

    /// The game (tests read its state).
    pub fn game(&self) -> &G {
        &self.game
    }

    /// The table's counters.
    pub fn events(&self) -> &Events {
        &self.events
    }

    /// The routing counters.
    pub fn counts(&self) -> &RoomCounts {
        &self.counts
    }

    /// Is `who` a member here?
    pub fn has(&self, who: &Identity) -> bool {
        self.members.contains_key(who)
    }

    /// The owner a member is bound to.
    pub fn owner_of(&self, who: &Identity) -> Option<Owner> {
        self.members.get(who).map(|m| m.owner)
    }

    /// One live step.
    pub fn tick(&mut self) {
        self.tick += 1;
        let mut rec: Option<Vec<Input>> = self.record.as_ref().map(|_| Vec::new());

        // 0 CONTROL (and, for shards, the shard FIFO: migrations,
        //   forwards, fences — in send order).
        while let Ok(c) = self.control_rx.try_recv() {
            self.control(c, &mut rec);
        }
        self.sweep();

        // 1 RESPONSES
        let mut responses = Vec::new();
        while let Ok(c) = self.completions_rx.try_recv() {
            responses.push(c);
        }
        if let Some(r) = &mut rec {
            r.push(Input::Responses(responses.clone()));
        }

        // 2 EVENTS — the room's own mailbox; one empty try_recv when idle.
        let mut drained = Vec::new();
        while let Ok(m) = self.events_rx.try_recv() {
            drained.push(m);
        }
        if let Some(r) = &mut rec {
            r.push(Input::Events(drained.clone()));
        }
        self.apply_events(drained);

        // 3 READ — the members' own input channels (bounded per member).
        let mut read: Vec<(Identity, Action)> = Vec::new();
        for (who, m) in &mut self.members {
            if let Some(rx) = &mut m.input {
                for _ in 0..READ_BUDGET {
                    match rx.try_recv() {
                        Ok(a) => read.push((who.clone(), a)),
                        Err(_) => break,
                    }
                }
            }
        }
        self.counts.inputs_read += read.len() as u64;
        if let Some(r) = &mut rec {
            r.push(Input::Read(read.clone()));
        }
        let actions = self.owned(&read);

        // 4-6 CONVERT + SYSTEMS (+ FLUSH in the engine)
        if let Some(r) = &mut rec {
            r.push(Input::Systems);
        }
        self.systems(&responses, &actions);

        // 7 tick end: untaken events dropped + counted; then BROADCAST.
        if let Some(r) = &mut rec {
            r.push(Input::EndTick);
        }
        self.events.end_tick();

        if let Some(inputs) = rec {
            let hash = self.hash();
            if let Some(log) = &mut self.record {
                log.push(TickRecord {
                    tick: self.tick,
                    inputs,
                    hash,
                });
            }
        }
    }

    /// Replay a record through a fresh room and game: the hash per tick.
    pub fn replay(id: RoomId, game: G, log: &[TickRecord]) -> Vec<u64> {
        let (mut room, _keep) = Self::new(id, game, Caps::default());
        let mut hashes = Vec::new();
        for t in log {
            room.tick += 1;
            let mut responses = Vec::new();
            let mut actions = Vec::new();
            for i in &t.inputs {
                match i {
                    Input::Control(c) => room.control_rec(c.clone()),
                    Input::Responses(v) => responses = v.clone(),
                    Input::Events(v) => room.apply_events(v.clone()),
                    Input::Read(v) => actions = room.owned(v),
                    Input::Systems => room.systems(&responses, &actions),
                    Input::EndTick => room.events.end_tick(),
                }
            }
            hashes.push(room.hash());
        }
        hashes
    }

    fn owned(&self, read: &[(Identity, Action)]) -> Vec<(Owner, Action)> {
        read.iter()
            .filter_map(|(who, a)| self.members.get(who).map(|m| (m.owner, a.clone())))
            .collect()
    }

    fn systems(&mut self, responses: &[Completion], actions: &[(Owner, Action)]) {
        self.game.update(Io {
            tick: self.tick,
            events: &mut self.events,
            responses,
            actions,
        });
    }

    fn hash(&self) -> u64 {
        let mut h = Fnv::default();
        h.u64(self.tick);
        self.game.hash(&mut h);
        h.0
    }

    fn mint(&mut self) -> Owner {
        let o = Owner(self.next_owner);
        self.next_owner += 1;
        o
    }

    fn control(&mut self, c: Control, rec: &mut Option<Vec<Input>>) {
        let note = |rec: &mut Option<Vec<Input>>, c: ControlRec| {
            if let Some(r) = rec {
                r.push(Input::Control(c));
            }
        };
        match c {
            Control::Join { identity, conn, input, carried } => {
                let owner = self.join(identity.clone(), conn, Some(input), carried);
                note(rec, ControlRec::Join { identity, conn, owner, carried });
            }
            Control::Leave { identity } => {
                note(rec, ControlRec::Leave { identity: identity.clone() });
                self.control_rec(ControlRec::Leave { identity });
            }
            Control::Park { identity } => {
                note(rec, ControlRec::Park { identity: identity.clone() });
                self.control_rec(ControlRec::Park { identity });
            }
            Control::Resume { identity, conn, input } => {
                if let Some(m) = self.members.get_mut(&identity) {
                    m.conn = conn;
                    m.input = Some(input);
                }
                note(rec, ControlRec::Resume { identity, conn });
            }
            Control::MigrateIn { identity, conn, input, mig, carried } => self.migrate_in(identity, conn, input, mig, carried),
            Control::Forward(d) => self.route_in(d, true),
            Control::Fence { identity, mig } => self.fence(identity, mig),
        }
    }

    /// CONTROL from a record (replay) — and the shared arms of the live one.
    fn control_rec(&mut self, c: ControlRec) {
        match c {
            ControlRec::Join { identity, conn, carried, .. } => {
                self.join(identity, conn, None, carried);
            }
            ControlRec::Leave { identity } => {
                if let Some(m) = self.members.remove(&identity) {
                    self.game.leave(m.owner);
                }
            }
            ControlRec::Park { identity } => {
                if let Some(m) = self.members.get_mut(&identity) {
                    m.input = None;
                }
            }
            ControlRec::Resume { identity, conn } => {
                if let Some(m) = self.members.get_mut(&identity) {
                    m.conn = conn;
                }
            }
        }
    }

    fn join(&mut self, identity: Identity, conn: ConnId, input: Option<mpsc::Receiver<Action>>, carried: u64) -> Owner {
        let owner = self.mint();
        self.game.join(owner, &identity, carried);
        self.members.insert(identity, Member { owner, conn, input });
        owner
    }

    fn migrate_in(&mut self, identity: Identity, conn: ConnId, input: Option<mpsc::Receiver<Action>>, mig: u64, carried: u64) {
        self.join(identity.clone(), conn, input, carried);
        let Some(sh) = &mut self.shard else { return };
        sh.migs.insert(identity.clone(), mig);
        let report = RouterMsg::Relocated {
            identity: identity.clone(),
            room: self.id,
            shard: sh.index,
            mig,
        };
        match self.router.as_ref().map(|r| r.try_send(report)) {
            Some(Ok(())) => {
                // Hold the router's direct legs until the source's fence.
                sh.holds.insert(
                    identity,
                    Hold {
                        mig,
                        since: self.tick,
                        held: Vec::new(),
                    },
                );
            }
            _ => {
                // The router keeps sending to the source, which forwards
                // until its tombstone expires: no seal will come, no hold.
                self.counts.relocations_refused += 1;
            }
        }
    }

    /// Move a member to shard `to` (the engine decides this at the seam,
    /// after SYSTEMS; the test calls it between steps).
    pub fn migrate_out(&mut self, who: &Identity, to: u16) {
        let Some(m) = self.members.remove(who) else { return };
        let carried = self.game.leave(m.owner);
        let Some(sh) = &mut self.shard else { return };
        let mig = sh.migs.remove(who).unwrap_or(0) + 1;
        let target = sh.neighbors[to as usize].clone();
        // The MigrateIn FIRST on the FIFO; then what an unfinished incoming
        // fence still held, in order.
        let _ = target.try_send(Control::MigrateIn {
            identity: who.clone(),
            conn: m.conn,
            input: m.input,
            mig,
            carried,
        });
        let mut awaiting_in = None;
        if let Some(h) = sh.holds.remove(who) {
            awaiting_in = Some(h.mig);
            for d in h.held {
                match target.try_send(Control::Forward(d)) {
                    Ok(()) => self.counts.forwarded += 1,
                    Err(_) => self.counts.forward_refused += 1,
                }
            }
        }
        sh.tombs.insert(
            who.clone(),
            Tomb {
                to,
                mig_out: mig,
                until: self.tick + TOMB_TTL,
                awaiting_in,
                sealed: false,
            },
        );
    }

    fn sweep(&mut self) {
        let tick = self.tick;
        let Some(sh) = &mut self.shard else { return };
        let before = sh.tombs.len();
        sh.tombs.retain(|_, t| t.until > tick);
        self.counts.tombstones_expired += (before - sh.tombs.len()) as u64;
        let late: Vec<Identity> = sh.holds.iter().filter(|(_, h)| tick - h.since > FENCE_TTL).map(|(k, _)| k.clone()).collect();
        for who in late {
            self.counts.fence_timeouts += 1;
            self.release(&who);
        }
    }

    fn release(&mut self, who: &Identity) {
        let Some(h) = self.shard.as_mut().and_then(|sh| sh.holds.remove(who)) else {
            return;
        };
        self.counts.fence_released += h.held.len() as u64;
        for d in h.held {
            self.route_in(d, true);
        }
    }

    fn fence(&mut self, identity: Identity, mig: u64) {
        let Some(sh) = &mut self.shard else {
            self.counts.fence_lost += 1;
            return;
        };
        if sh.holds.get(&identity).is_some_and(|h| h.mig == mig) {
            self.release(&identity);
            return;
        }
        if let Some(t) = sh.tombs.get_mut(&identity)
            && t.awaiting_in == Some(mig)
        {
            t.awaiting_in = None;
            if t.sealed {
                let (to, out) = (t.to, t.mig_out);
                sh.tombs.remove(&identity);
                let _ = sh.neighbors[to as usize].try_send(Control::Fence { identity, mig: out });
            }
            return;
        }
        self.counts.fence_lost += 1;
    }

    fn seal(&mut self, identity: Identity, mig: u64) {
        let Some(sh) = &mut self.shard else {
            self.counts.fence_lost += 1;
            return;
        };
        match sh.tombs.get_mut(&identity) {
            Some(t) if t.mig_out == mig => {
                t.sealed = true;
                if t.awaiting_in.is_none() {
                    let to = t.to;
                    sh.tombs.remove(&identity);
                    let _ = sh.neighbors[to as usize].try_send(Control::Fence { identity, mig });
                }
            }
            _ => self.counts.fence_lost += 1,
        }
    }

    /// EVENTS: what the room's own mailbox held, in order.
    fn apply_events(&mut self, drained: Vec<RoomEventMsg>) {
        for m in drained {
            match m {
                RoomEventMsg::Event(d) => self.route_in(d, false),
                RoomEventMsg::Seal { identity, mig } => self.seal(identity, mig),
            }
        }
    }

    /// One WORLD leg: into the table, behind a fence, on to a migration's
    /// target, or counted.
    fn route_in(&mut self, d: Delivered, forwarded: bool) {
        let who = match &d.to {
            Addressee::Room => {
                self.events.admit(d.op, &d.body, Addressee::Room, Owner::ACTOR, d.publisher, d.seq);
                return;
            }
            Addressee::Player(who) => who.clone(),
        };
        if !forwarded
            && let Some(h) = self.shard.as_mut().and_then(|sh| sh.holds.get_mut(&who))
        {
            if h.held.len() >= HOLD_CAP {
                self.counts.fence_overflow += 1;
            } else {
                h.held.push(d);
                self.counts.fence_held += 1;
            }
            return;
        }
        if let Some(m) = self.members.get(&who) {
            self.events.admit(d.op, &d.body, d.to.clone(), m.owner, d.publisher, d.seq);
            return;
        }
        if let Some(sh) = &self.shard
            && let Some(t) = sh.tombs.get(&who)
        {
            match sh.neighbors[t.to as usize].try_send(Control::Forward(d)) {
                Ok(()) => self.counts.forwarded += 1,
                Err(TrySendError::Full(_) | TrySendError::Closed(_)) => self.counts.forward_refused += 1,
            }
            return;
        }
        self.counts.not_here += 1;
    }
}
