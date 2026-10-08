//! The publishing handle (docs/EVENTS.md §5): any server code that holds
//! one may push — a service, an admin task, a room. Clients never hold
//! one (there is no client → server PUSH; §13).
//!
//! Every push is ONE `try_send` into the router's bounded mailbox: it
//! never awaits, so a room may push from a system and a service from its
//! call. The payload is encoded ONCE, here, whatever the fan-out
//! (`push_many` to 10 000 identities is one encode and one message; the
//! router shares the `Bytes` by reference counting).

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

use bytes::Bytes;
use tokio::sync::mpsc::{self, error::TrySendError};

use crate::contract::{Dest, Event, Identity, PublisherId, RoomId};
use crate::router::RouterMsg;

/// Who a push is addressed to: only an identity, a room, or everyone.
/// Topics, channels, guilds and parties are the game service's (it fans
/// out with `push_many`; §6).
#[derive(Debug, Clone)]
pub enum Target {
    /// One identity.
    One(Identity),
    /// Many identities (one message, one encode).
    Many(Arc<[Identity]>),
    /// Every member of a room (CLIENT leg) / the room's logic (WORLD leg).
    Room(RoomId),
    /// Every session (CLIENT) / every room (WORLD).
    All,
}

/// One push, as the router receives it.
#[derive(Debug, Clone)]
pub struct Push {
    /// The publisher.
    pub publisher: PublisherId,
    /// The addressees.
    pub to: Target,
    /// The event's op.
    pub op: u16,
    /// Its legs.
    pub dest: Dest,
    /// The body, encoded once.
    pub body: Bytes,
}

/// Why a push was refused AT THE HANDLE (typed, immediate; everything
/// later is counted by the router and the actors downstream).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PushRefused {
    /// The router's mailbox is full (`events_refused_router_full`).
    RouterFull,
    /// The router is gone (server stopping; `events_refused_router_gone`).
    RouterGone,
}

/// The handle's own counters (atomics: the handle is `Clone` and pushed
/// from many tasks; nothing is shared but these cells).
#[derive(Debug, Default)]
pub struct PublisherStats {
    /// Pushes the router accepted.
    pub pushed: AtomicU64,
    /// Refused: router mailbox full.
    pub refused_router_full: AtomicU64,
    /// Refused: router gone.
    pub refused_router_gone: AtomicU64,
    /// Payload encodes (the spike's proof that `push_many` encodes once).
    pub encodes: AtomicU64,
}

/// A publishing handle. `Clone`; cheap.
#[derive(Debug, Clone)]
pub struct Publisher {
    id: PublisherId,
    router: mpsc::Sender<RouterMsg>,
    stats: Arc<PublisherStats>,
}

impl Publisher {
    /// A handle (the composition root mints them: `parts.publisher("bank")`).
    pub fn new(id: PublisherId, router: mpsc::Sender<RouterMsg>) -> Self {
        Self {
            id,
            router,
            stats: Arc::default(),
        }
    }

    /// Its id.
    pub fn id(&self) -> PublisherId {
        self.id
    }

    /// Its counters.
    pub fn stats(&self) -> &PublisherStats {
        &self.stats
    }

    /// Push `e` to one identity.
    pub fn push<E: Event>(&self, to: &Identity, e: &E) -> Result<(), PushRefused> {
        self.send::<E>(Target::One(to.clone()), e)
    }

    /// Push `e` to many identities: ONE encode, ONE router message.
    pub fn push_many<E: Event>(&self, to: &[Identity], e: &E) -> Result<(), PushRefused> {
        self.send::<E>(Target::Many(Arc::from(to)), e)
    }

    /// Push `e` to a room (its members' clients / its logic).
    pub fn push_room<E: Event>(&self, room: RoomId, e: &E) -> Result<(), PushRefused> {
        self.send::<E>(Target::Room(room), e)
    }

    /// Push `e` to everyone.
    pub fn push_all<E: Event>(&self, e: &E) -> Result<(), PushRefused> {
        self.send::<E>(Target::All, e)
    }

    fn send<E: Event>(&self, to: Target, e: &E) -> Result<(), PushRefused> {
        self.stats.encodes.fetch_add(1, Relaxed);
        let push = Push {
            publisher: self.id,
            to,
            op: E::OP,
            dest: E::TO,
            body: e.to_bytes(),
        };
        match self.router.try_send(RouterMsg::Push(push)) {
            Ok(()) => {
                self.stats.pushed.fetch_add(1, Relaxed);
                Ok(())
            }
            Err(TrySendError::Full(_)) => {
                self.stats.refused_router_full.fetch_add(1, Relaxed);
                Err(PushRefused::RouterFull)
            }
            Err(TrySendError::Closed(_)) => {
                self.stats.refused_router_gone.fetch_add(1, Relaxed);
                Err(PushRefused::RouterGone)
            }
        }
    }
}
