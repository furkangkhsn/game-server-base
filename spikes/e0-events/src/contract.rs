//! The typed contract of an event (docs/EVENTS.md §4): what `gsb-build`
//! generates from `option (gsb.event) = { op: N, to: WORLD|CLIENT|BOTH }`.
//!
//! The destination is a CONSTANT of the type, and it is also lifted into
//! two marker traits — [`WorldEvent`] (WORLD, BOTH) and [`ClientEvent`]
//! (CLIENT, BOTH) — so a room taking a CLIENT-only event, or a client
//! listening for a WORLD-only one, is a compile error, not a silent
//! empty list.

use std::fmt;
use std::sync::Arc;

use bytes::Bytes;

/// Where an event goes (the `.proto` declaration's `to:`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dest {
    /// The player's room logic reacts (balance, quest progress …).
    World,
    /// Straight to the client; no room ever sees it (chat, presence).
    Client,
    /// Both legs, stamped separately.
    Both,
}

impl Dest {
    /// Does the WORLD leg run?
    pub fn world(self) -> bool {
        matches!(self, Self::World | Self::Both)
    }
    /// Does the CLIENT leg run?
    pub fn client(self) -> bool {
        matches!(self, Self::Client | Self::Both)
    }
}

/// Could not decode a payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodeError;

/// A message that crosses a byte boundary (the wire, the record file).
/// Blanket-implemented for every prost message, like the engine's.
pub trait WireMessage: Sized + Send + 'static {
    /// Encode to bytes.
    fn to_bytes(&self) -> Bytes;
    /// Decode from bytes.
    fn from_bytes(b: &[u8]) -> Result<Self, DecodeError>;
}

impl<M: prost::Message + Default + Send + 'static> WireMessage for M {
    fn to_bytes(&self) -> Bytes {
        Bytes::from(self.encode_to_vec())
    }
    fn from_bytes(b: &[u8]) -> Result<Self, DecodeError> {
        M::decode(b).map_err(|_| DecodeError)
    }
}

/// An event type: generated per `.proto` message carrying `(gsb.event)`.
pub trait Event: WireMessage + Clone + fmt::Debug {
    /// The event's op (the game band; one op space with requests).
    const OP: u16;
    /// Where it goes.
    const TO: Dest;
}

/// An event a room may take (`to: WORLD` or `to: BOTH`).
pub trait WorldEvent: Event {}

/// An event a client may listen for (`to: CLIENT` or `to: BOTH`).
pub trait ClientEvent: Event {}

/// A verified identity (the ticket's player). Never taken from a payload.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Identity(Arc<str>);

impl Identity {
    /// Wrap an identity string.
    pub fn new(s: &str) -> Self {
        Self(Arc::from(s))
    }
    /// The text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Identity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}", &*self.0)
    }
}

/// A room id.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RoomId(pub u64);

/// A connection (session) id.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ConnId(pub u64);

/// Who published: one id per [`crate::publisher::Publisher`] handle
/// (a service, an admin task, a room). The engine's own events are
/// [`PublisherId::ENGINE`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PublisherId(pub u32);

impl PublisherId {
    /// The engine itself.
    pub const ENGINE: Self = Self(0);
}

/// What an event's result hangs on, as in `Calls` (an opaque `u64`;
/// bevy/hecs bind it to an entity).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Owner(pub u64);

impl Owner {
    /// The actor itself (room-wide and server-wide events).
    pub const ACTOR: Self = Self(u64::MAX);
}

/// Which of an event's two legs a sequence number counts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Leg {
    /// The room leg.
    World,
    /// The client leg.
    Client,
}
