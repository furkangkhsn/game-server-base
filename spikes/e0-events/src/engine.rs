//! Engine-originated events (docs/EVENTS.md §5.4): what the registry
//! already learns about sessions and rooms, offered to SERVICES (a
//! friends service's presence source). They never go to clients and
//! never enter a room: rooms learn joins and leaves from CONTROL.
//!
//! A subscriber is a bounded receiver; the router `try_send`s and counts
//! a full or gone subscriber per name. Losing one is a counted loss like
//! any other — a presence service that must not miss one sizes its
//! mailbox, or re-reads the truth (`gsb` keeps no presence history).

use tokio::sync::mpsc::{self, error::TrySendError};

use crate::contract::{Identity, RoomId};

/// What the engine tells subscribers. `#[non_exhaustive]`: the set grows
/// additively (a service matches with a `_` arm).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum EngineEvent {
    /// The identity's first authenticated session opened (a second
    /// session superseding the first is not news).
    Online { identity: Identity },
    /// Its session ended and nothing holds it (not parked).
    Offline { identity: Identity },
    /// It joined a room.
    Joined { identity: Identity, room: RoomId },
    /// It left a room (or went offline in one).
    Left { identity: Identity, room: RoomId },
    /// The transport died; the room parked its entity (`room`: where).
    Parked { identity: Identity, room: Option<RoomId> },
    /// A parked identity came back on a new session.
    Resumed { identity: Identity, room: Option<RoomId> },
}

impl EngineEvent {
    fn kind(&self) -> Interest {
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

/// Which engine events a subscriber wants (a bit set).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Interest(pub u32);

impl Interest {
    pub const ONLINE: Self = Self(1);
    pub const OFFLINE: Self = Self(2);
    pub const JOINED: Self = Self(4);
    pub const LEFT: Self = Self(8);
    pub const PARKED: Self = Self(16);
    pub const RESUMED: Self = Self(32);
    /// Presence: online, offline, parked, resumed.
    pub const PRESENCE: Self = Self(1 | 2 | 16 | 32);
    /// Everything.
    pub const ALL: Self = Self(u32::MAX);

    /// Union.
    pub const fn or(self, o: Self) -> Self {
        Self(self.0 | o.0)
    }
    fn has(self, o: Self) -> bool {
        self.0 & o.0 != 0
    }
}

/// One subscriber's counters.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct SubscriberCounts {
    /// Delivered.
    pub sent: u64,
    /// Refused: its mailbox full (`engine_events_refused_full{subscriber}`).
    pub refused_full: u64,
    /// Refused: it is gone.
    pub refused_gone: u64,
}

struct Sub {
    name: &'static str,
    interest: Interest,
    tx: mpsc::Sender<EngineEvent>,
    counts: SubscriberCounts,
}

/// The subscriber list (in the registry).
#[derive(Default)]
pub struct Subscribers {
    subs: Vec<Sub>,
}

impl Subscribers {
    /// Add one.
    pub fn add(&mut self, name: &'static str, interest: Interest, cap: usize) -> mpsc::Receiver<EngineEvent> {
        let (tx, rx) = mpsc::channel(cap);
        self.subs.push(Sub {
            name,
            interest,
            tx,
            counts: SubscriberCounts::default(),
        });
        rx
    }

    /// Offer `e` to every interested subscriber. With none subscribed this
    /// is one empty iteration (the idle cost).
    pub fn emit(&mut self, e: EngineEvent) {
        let k = e.kind();
        for s in &mut self.subs {
            if !s.interest.has(k) {
                continue;
            }
            match s.tx.try_send(e.clone()) {
                Ok(()) => s.counts.sent += 1,
                Err(TrySendError::Full(_)) => s.counts.refused_full += 1,
                Err(TrySendError::Closed(_)) => s.counts.refused_gone += 1,
            }
        }
    }

    /// A subscriber's counters by name.
    pub fn counts(&self, name: &str) -> Option<&SubscriberCounts> {
        self.subs.iter().find(|s| s.name == name).map(|s| &s.counts)
    }
}
