//! The publishing handle (`docs/EVENTS.md` §5): whoever holds one may
//! push — a service, an admin task, a room (§20 E10). Clients never hold
//! one: there is no client → server PUSH (§13).
//!
//! Every push is ONE `try_send` into the registry's bounded mailbox
//! (the router): it never awaits, so a room may push from a system and
//! a service from inside a call. The body is encoded ONCE, here,
//! whatever the fan-out — `push_many` to 10 000 identities is one encode
//! and one message; the router shares the `Bytes` by reference count.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

use bytes::Bytes;

use crate::calls::Identity;
use crate::channel::{Mailbox, TrySend};
use crate::events::{Dest, Event};
use crate::id::RoomId;
use crate::metrics::PublisherCounts;
use crate::registry::RegistryMsg;

/// The default of a publishing handle's `max_event_bytes` (§20 E3):
/// well under every transport's frame bound, so a publisher's mistake
/// cannot produce a PUSH frame the client must refuse.
pub const DEFAULT_MAX_EVENT_BYTES: usize = 64 * 1024;

/// A publisher's id: the PUSH frame's `publisher`, the key of the
/// sequence numbers. `0` is reserved for the engine itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct PublisherId(pub u32);

impl PublisherId {
    /// The engine's own events (round e4).
    pub const ENGINE: PublisherId = PublisherId(0);
}

/// Who a push is addressed to (`docs/EVENTS.md` §5.2): an identity, many
/// identities, a room, or everyone — nothing else. Channels, guilds and
/// parties are the game service's (it fans out with `push_many`).
#[derive(Debug, Clone)]
pub enum Target {
    /// One identity.
    One(Identity),
    /// Many identities (one message, one encode). A duplicate is not
    /// removed (§20 E4): it gets two legs, two numbers.
    Many(Arc<[Identity]>),
    /// A room: its members' clients (CLIENT leg) and its logic, every
    /// shard once (WORLD leg, `Addressee::Room`).
    Room(RoomId),
    /// Every authenticated session (CLIENT) and every room (WORLD).
    All,
}

/// One push, as the router takes it from its mailbox. Built by a
/// [`Publisher`] only.
pub struct Push {
    pub(crate) publisher: PublisherId,
    pub(crate) to: Target,
    pub(crate) op: u16,
    pub(crate) dest: Dest,
    pub(crate) body: Bytes,
}

impl std::fmt::Debug for Push {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Push")
            .field("publisher", &self.publisher)
            .field("to", &self.to)
            .field("op", &self.op)
            .field("dest", &self.dest)
            .field("bytes", &self.body.len())
            .finish()
    }
}

/// Why a push was refused AT THE HANDLE — typed and immediate. Every loss
/// after the router took the push (offline, parked, a full room, an old
/// client …) is counted by name downstream and never comes back here
/// (at most once; a feedback path would be a second channel).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PushRefused {
    /// The encoded body is larger than the handle's `max_event_bytes`
    /// (`events_refused_too_large`).
    TooLarge {
        /// The encoded size.
        size: usize,
        /// The handle's bound.
        max: usize,
    },
    /// The router's mailbox is full (`events_refused_router_full`).
    RouterFull,
    /// The router is gone — the server is stopping
    /// (`events_refused_router_gone`).
    RouterGone,
}

impl std::fmt::Display for PushRefused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PushRefused::TooLarge { size, max } => {
                write!(
                    f,
                    "event body of {size} bytes exceeds max_event_bytes {max}"
                )
            }
            PushRefused::RouterFull => f.write_str("the event router's mailbox is full"),
            PushRefused::RouterGone => f.write_str("the event router is gone"),
        }
    }
}

impl std::error::Error for PushRefused {}

/// A handle's counters: atomics, shared by its clones (a handle is pushed
/// from many tasks; these cells are all they share). The collector reads
/// them at each report (the [`ServiceStats`](crate::calls::ServiceStats)
/// pattern).
#[derive(Debug, Default)]
pub struct PublisherStats {
    pub(crate) events_pushed: AtomicU64,
    pub(crate) events_refused_router_full: AtomicU64,
    pub(crate) events_refused_router_gone: AtomicU64,
    pub(crate) events_refused_too_large: AtomicU64,
    /// Bodies encoded (not a metric: the proof that a fan-out encodes
    /// once).
    pub(crate) encodes: AtomicU64,
}

impl PublisherStats {
    /// The counters, read now.
    pub fn counts(&self) -> PublisherCounts {
        PublisherCounts {
            events_pushed: self.events_pushed.load(Relaxed),
            events_refused_router_full: self.events_refused_router_full.load(Relaxed),
            events_refused_router_gone: self.events_refused_router_gone.load(Relaxed),
            events_refused_too_large: self.events_refused_too_large.load(Relaxed),
        }
    }

    /// How many bodies the handle encoded (one per push, whatever its
    /// fan-out).
    pub fn encodes(&self) -> u64 {
        self.encodes.load(Relaxed)
    }
}

/// A publishing handle (`docs/EVENTS.md` §5.1). `Clone`, cheap; every
/// clone shares the id and the counters. Minted by the composition root
/// ([`Publishers::publisher`], `RegistryParts::publisher(name)` in
/// `gsb-server`) and handed to a service's constructor, a room's factory,
/// an admin task.
///
/// A replay's detached handle (pushes go nowhere, uncounted: a room's
/// pushes are its output, not its input — §5.1, §11) is round e5.
#[derive(Clone)]
pub struct Publisher {
    id: PublisherId,
    name: Arc<str>,
    router: Mailbox<RegistryMsg>,
    stats: Arc<PublisherStats>,
    max_event_bytes: usize,
}

impl std::fmt::Debug for Publisher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Publisher")
            .field("id", &self.id)
            .field("name", &self.name)
            .field("max_event_bytes", &self.max_event_bytes)
            .finish()
    }
}

impl Publisher {
    /// Its id (the PUSH frame's `publisher`).
    pub fn id(&self) -> PublisherId {
        self.id
    }

    /// Its name (the metric label).
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Its counters.
    pub fn stats(&self) -> &PublisherStats {
        &self.stats
    }

    /// Push `e` to one identity (its client, its room's logic, or both —
    /// as `E`'s `.proto` declares).
    ///
    /// # Errors
    ///
    /// [`PushRefused`]: the immediate, typed refusals (counted).
    pub fn push<E: Event>(&self, to: &Identity, e: &E) -> Result<(), PushRefused> {
        self.send::<E>(Target::One(to.clone()), e)
    }

    /// Push `e` to many identities: ONE encode, ONE router message.
    ///
    /// # Errors
    ///
    /// [`PushRefused`], as [`Self::push`].
    pub fn push_many<E: Event>(&self, to: &[Identity], e: &E) -> Result<(), PushRefused> {
        self.send::<E>(Target::Many(Arc::from(to)), e)
    }

    /// Push `e` to a room: its members' clients (CLIENT leg) and its
    /// logic, every shard once (WORLD leg, `Addressee::Room`).
    ///
    /// # Errors
    ///
    /// [`PushRefused`], as [`Self::push`].
    pub fn push_room<E: Event>(&self, room: RoomId, e: &E) -> Result<(), PushRefused> {
        self.send::<E>(Target::Room(room), e)
    }

    /// Push `e` to everyone: every authenticated session (CLIENT leg) and
    /// every room (WORLD leg, room-wide).
    ///
    /// # Errors
    ///
    /// [`PushRefused`], as [`Self::push`].
    pub fn push_all<E: Event>(&self, e: &E) -> Result<(), PushRefused> {
        self.send::<E>(Target::All, e)
    }

    fn send<E: Event>(&self, to: Target, e: &E) -> Result<(), PushRefused> {
        self.stats.encodes.fetch_add(1, Relaxed);
        let body = Bytes::from(e.to_wire());
        if body.len() > self.max_event_bytes {
            self.stats.events_refused_too_large.fetch_add(1, Relaxed);
            return Err(PushRefused::TooLarge {
                size: body.len(),
                max: self.max_event_bytes,
            });
        }
        let push = Push {
            publisher: self.id,
            to,
            op: E::OP,
            dest: E::TO,
            body,
        };
        match crate::channel::try_send(&self.router, RegistryMsg::Push(push)) {
            TrySend::Sent => {
                self.stats.events_pushed.fetch_add(1, Relaxed);
                Ok(())
            }
            TrySend::Full => {
                self.stats.events_refused_router_full.fetch_add(1, Relaxed);
                Err(PushRefused::RouterFull)
            }
            TrySend::Closed => {
                self.stats.events_refused_router_gone.fetch_add(1, Relaxed);
                Err(PushRefused::RouterGone)
            }
        }
    }
}

/// What the metrics collector reads per publisher: its name, its id and
/// its counters ([`Publishers::sources`]).
pub type PublisherSource = (Arc<str>, PublisherId, Arc<PublisherStats>);

/// The composition root's publisher mint: one per server, over the
/// registry's mailbox. Ids are handed out from 1 in minting order (0 is
/// the engine's); minting a name twice hands back the same handle (one
/// id, one set of counters).
#[derive(Debug)]
pub struct Publishers {
    router: Mailbox<RegistryMsg>,
    max_event_bytes: usize,
    minted: Vec<Publisher>,
}

impl Publishers {
    /// A mint whose handles push into `router` (the registry's mailbox)
    /// and refuse bodies over `max_event_bytes`.
    pub fn new(router: Mailbox<RegistryMsg>, max_event_bytes: usize) -> Self {
        Self {
            router,
            max_event_bytes,
            minted: Vec::new(),
        }
    }

    /// The handle named `name` (the metric label and the log's name).
    pub fn publisher(&mut self, name: &str) -> Publisher {
        if let Some(p) = self.minted.iter().find(|p| &*p.name == name) {
            return p.clone();
        }
        let id = PublisherId(self.minted.len() as u32 + 1);
        let p = Publisher {
            id,
            name: Arc::from(name),
            router: self.router.clone(),
            stats: Arc::default(),
            max_event_bytes: self.max_event_bytes,
        };
        self.minted.push(p.clone());
        p
    }

    /// Every handle minted so far, for the metrics collector.
    pub fn sources(&self) -> Vec<PublisherSource> {
        self.minted
            .iter()
            .map(|p| (Arc::clone(&p.name), p.id, Arc::clone(&p.stats)))
            .collect()
    }
}
