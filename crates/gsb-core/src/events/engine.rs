//! Engine-originated events (`docs/EVENTS.md` §5.4, round e4): what the
//! registry already learns about sessions and rooms — an identity came
//! online, joined a room, left it, parked, resumed, went offline —
//! offered to SERVICES (a friends service's presence source). Never to
//! clients, never into a room: rooms learn joins and leaves from CONTROL.
//!
//! A subscriber is a bounded channel; the registry `try_send`s each
//! [`EngineNotice`] (it never awaits) and counts a full or gone
//! subscriber under the subscriber's NAME
//! ([`EngineCounts`](crate::metrics::EngineCounts):
//! `engine_events_{sent, refused_full, refused_gone}`). Every notice
//! carries the subscriber's own sequence number, spent whether the notice
//! was delivered or not: a subscriber that sees a gap lost something and
//! re-reads the truth ([`Presence`](crate::events::Presence), §20 E12) —
//! the engine keeps no presence history.
//!
//! Two ways to subscribe (the composition root's; `gsb-server`'s
//! `RegistryParts`):
//!
//! - a `#[gsb_core::service]` trait with one `#[gsb(engine_events)]`
//!   method, registered with
//!   [`Setup::engine_events`](crate::calls::Setup::engine_events): the
//!   host runs the method for every notice, in order, on a task of its
//!   own (beside the service's call loop, never through the tower stack —
//!   there is no answer);
//! - a plain receiver for any async task: [`EngineSubscribers::subscribe`]
//!   (`parts.engine_events(name, interest, cap)`).
//!
//! **What the events mean** (derived from the registry's own connection
//! table, once per registry message that can change it — the same facts
//! event routing reads, so the two can never disagree):
//!
//! - an identity is ONLINE while the table holds a row for it — an
//!   authenticated session, live or parked, or a park row; [`Online`]
//!   when its first row appears, [`Offline`] when its last one goes. A
//!   second session of an online identity is not news (no `Online`);
//! - it is LIVE while one of its rows is an authenticated session whose
//!   transport is up; an online identity that is not live is PARKED
//!   ([`Parked`], [`Resumed`] when a live session comes back — at that
//!   session's AUTH, before it rejoins anything). The registry learns a
//!   transport death before the room decides despawn-vs-hold, so a room
//!   that declines to park is seen as `Parked` then `Left` + `Offline`;
//! - its ROOM is where its WORLD legs go — its live membership, else its
//!   parked one ([`Joined`] / [`Left`]).
//!
//! Anonymous sessions (an empty identity) produce nothing. One registry
//! message yields an identity's events in this order: `Online`, `Left`,
//! `Joined`, `Parked` / `Resumed`, and `Left` before `Offline`.
//! Engine events are not recorded (services are outside the determinism
//! boundary, SERVICES §10.4).
//!
//! [`Online`]: EngineEvent::Online
//! [`Offline`]: EngineEvent::Offline
//! [`Joined`]: EngineEvent::Joined
//! [`Left`]: EngineEvent::Left
//! [`Parked`]: EngineEvent::Parked
//! [`Resumed`]: EngineEvent::Resumed

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

use crate::calls::Identity;
use crate::channel::{Inbox, Mailbox, TrySend};
use crate::id::RoomId;
use crate::metrics::EngineCounts;

#[cfg(test)]
mod tests;

/// What the engine tells its subscribers. `#[non_exhaustive]`: the set
/// grows additively — a subscriber matches with a `_` arm.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum EngineEvent {
    /// The identity's first authenticated session opened (it had no row).
    Online {
        /// Who.
        identity: Identity,
    },
    /// Its last row went: no session, nothing parked.
    Offline {
        /// Who.
        identity: Identity,
    },
    /// It joined a room (its WORLD legs go there now).
    Joined {
        /// Who.
        identity: Identity,
        /// Where.
        room: RoomId,
    },
    /// It left a room — a leave, the room ending, or going offline in it.
    Left {
        /// Who.
        identity: Identity,
        /// Which room.
        room: RoomId,
    },
    /// Its transport died and nothing live is left of it: its entity
    /// waits in `room` (when it has one) for a resume — or for the room's
    /// despawn.
    Parked {
        /// Who.
        identity: Identity,
        /// Where its entity waits (`None`: it was in no room).
        room: Option<RoomId>,
    },
    /// A parked identity is live again: a new session authenticated.
    Resumed {
        /// Who.
        identity: Identity,
        /// Where its entity waits (the room its resume returns to).
        room: Option<RoomId>,
    },
}

impl EngineEvent {
    /// The identity it is about.
    pub fn identity(&self) -> &Identity {
        match self {
            Self::Online { identity }
            | Self::Offline { identity }
            | Self::Joined { identity, .. }
            | Self::Left { identity, .. }
            | Self::Parked { identity, .. }
            | Self::Resumed { identity, .. } => identity,
        }
    }

    /// Its kind as an [`Interest`] bit.
    pub fn kind(&self) -> Interest {
        match self {
            Self::Online { .. } => Interest::ONLINE,
            Self::Offline { .. } => Interest::OFFLINE,
            Self::Joined { .. } => Interest::JOINED,
            Self::Left { .. } => Interest::LEFT,
            Self::Parked { .. } => Interest::PARKED,
            Self::Resumed { .. } => Interest::RESUMED,
        }
    }
}

/// One delivered engine event: the subscriber's sequence number (from 1,
/// spent by every event offered to it — a gap is a loss, counted
/// `engine_events_refused_*`) and the event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EngineNotice {
    /// This subscriber's number for it.
    pub seq: u64,
    /// What happened.
    pub event: EngineEvent,
}

impl From<EngineNotice> for EngineEvent {
    fn from(n: EngineNotice) -> Self {
        n.event
    }
}

/// Which engine events a subscriber wants (a bit set).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Interest(u32);

impl Interest {
    /// Nothing (a subscriber that hears nothing).
    pub const NONE: Self = Self(0);
    /// [`EngineEvent::Online`].
    pub const ONLINE: Self = Self(1);
    /// [`EngineEvent::Offline`].
    pub const OFFLINE: Self = Self(1 << 1);
    /// [`EngineEvent::Joined`].
    pub const JOINED: Self = Self(1 << 2);
    /// [`EngineEvent::Left`].
    pub const LEFT: Self = Self(1 << 3);
    /// [`EngineEvent::Parked`].
    pub const PARKED: Self = Self(1 << 4);
    /// [`EngineEvent::Resumed`].
    pub const RESUMED: Self = Self(1 << 5);
    /// Presence: online, offline, parked, resumed.
    pub const PRESENCE: Self = Self(1 | 1 << 1 | 1 << 4 | 1 << 5);
    /// Every event, including kinds added later.
    pub const ALL: Self = Self(u32::MAX);

    /// The union.
    pub const fn or(self, o: Self) -> Self {
        Self(self.0 | o.0)
    }

    /// Whether any bit of `o` is in `self`.
    pub const fn has(self, o: Self) -> bool {
        self.0 & o.0 != 0
    }

    /// The raw bits.
    pub const fn bits(self) -> u32 {
        self.0
    }
}

impl std::ops::BitOr for Interest {
    type Output = Self;
    fn bitor(self, o: Self) -> Self {
        self.or(o)
    }
}

/// A plain subscriber's receiving end
/// ([`EngineSubscribers::subscribe`]).
pub type EngineReceiver = Inbox<EngineNotice>;

/// One subscriber's counters: atomics the registry bumps and the metrics
/// collector reads (the [`PublisherStats`](crate::events::PublisherStats)
/// pattern — one writer, so `Relaxed` is all it needs).
#[derive(Debug, Default)]
pub struct EngineSubscriberStats {
    pub(crate) sent: AtomicU64,
    pub(crate) refused_full: AtomicU64,
    pub(crate) refused_gone: AtomicU64,
}

impl EngineSubscriberStats {
    /// The counters, read now.
    pub fn counts(&self) -> EngineCounts {
        EngineCounts {
            engine_events_sent: self.sent.load(Relaxed),
            engine_events_refused_full: self.refused_full.load(Relaxed),
            engine_events_refused_gone: self.refused_gone.load(Relaxed),
        }
    }
}

/// What the metrics collector reads per subscriber: its name and its
/// counters ([`EngineSubscribers::sources`]).
pub type EngineSubscriberSource = (Arc<str>, Arc<EngineSubscriberStats>);

struct Subscriber {
    name: Arc<str>,
    interest: Interest,
    tx: Mailbox<EngineNotice>,
    /// The last number spent.
    seq: u64,
    stats: Arc<EngineSubscriberStats>,
}

/// The engine-event subscribers: built at the composition root, then
/// owned by the registry actor
/// ([`Registry::with_engine_events`](crate::registry::Registry::with_engine_events)).
#[derive(Default)]
pub struct EngineSubscribers {
    subs: Vec<Subscriber>,
}

impl std::fmt::Debug for EngineSubscribers {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_list()
            .entries(self.subs.iter().map(|s| (&*s.name, s.interest)))
            .finish()
    }
}

impl EngineSubscribers {
    /// No subscriber.
    pub fn new() -> Self {
        Self::default()
    }

    /// Subscribe a plain receiver: `interest`'s events, into a channel of
    /// `cap` (at least 1). `name` is the metric label
    /// (`gsb_engine_events_*_total{subscriber="<name>"}`); a name used
    /// twice is two subscribers whose counters add up under it.
    ///
    /// ```ignore
    /// let mut rx = subs.subscribe("presence-audit", Interest::ONLINE | Interest::OFFLINE, 1024);
    /// tokio::spawn(async move { while let Some(n) = rx.recv().await { audit(n).await } });
    /// ```
    pub fn subscribe(&mut self, name: &str, interest: Interest, cap: usize) -> EngineReceiver {
        let (tx, rx) = crate::channel::channel(cap.max(1));
        self.add(name, interest, tx);
        rx
    }

    /// Subscribe an existing sender (a hosted service's engine task,
    /// [`Services::take_engine_subscriptions`](crate::calls::Services::take_engine_subscriptions)).
    pub fn add(&mut self, name: &str, interest: Interest, tx: Mailbox<EngineNotice>) {
        let stats = match self.subs.iter().find(|s| &*s.name == name) {
            Some(s) => Arc::clone(&s.stats),
            None => Arc::default(),
        };
        self.subs.push(Subscriber {
            name: Arc::from(name),
            interest,
            tx,
            seq: 0,
            stats,
        });
    }

    /// Whether nobody subscribed (the registry then derives nothing: the
    /// idle cost is one branch per message).
    pub fn is_empty(&self) -> bool {
        self.subs.is_empty()
    }

    /// Every subscriber's name and counters, once per name, in
    /// subscription order (the metrics collector's).
    pub fn sources(&self) -> Vec<EngineSubscriberSource> {
        let mut out: Vec<EngineSubscriberSource> = Vec::new();
        for s in &self.subs {
            if !out.iter().any(|(n, _)| *n == s.name) {
                out.push((Arc::clone(&s.name), Arc::clone(&s.stats)));
            }
        }
        out
    }

    /// Offer `e` to every interested subscriber: its next number, one
    /// `try_send`, the ending counted under its name. Never awaits.
    pub(crate) fn emit(&mut self, e: &EngineEvent) {
        let kind = e.kind();
        for s in &mut self.subs {
            if !s.interest.has(kind) {
                continue;
            }
            s.seq += 1;
            let n = EngineNotice {
                seq: s.seq,
                event: e.clone(),
            };
            let cell = match crate::channel::try_send(&s.tx, n) {
                TrySend::Sent => &s.stats.sent,
                TrySend::Full => &s.stats.refused_full,
                TrySend::Closed => &s.stats.refused_gone,
            };
            cell.fetch_add(1, Relaxed);
        }
    }
}
