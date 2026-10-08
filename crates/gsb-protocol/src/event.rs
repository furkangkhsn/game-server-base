//! The typed event contract (`docs/EVENTS.md` §4.2): what `gsb-build`
//! generates for a `.proto` message carrying
//! `option (gsb.event) = { op: N, to: WORLD | CLIENT | BOTH }`.
//!
//! An event is a typed message server code pushes to an identity, a room
//! or everyone, nobody having asked for it. Its destination is a property
//! of the TYPE, declared once in the `.proto`, not an argument at the push
//! site: [`Event::TO`] is a constant, and it is lifted into two marker
//! traits — [`WorldEvent`] (`WORLD`, `BOTH`: a room may take it) and
//! [`ClientEvent`] (`CLIENT`, `BOTH`: a client may listen for it) — so a
//! room taking a client-only event, or a client listening for a
//! world-only one, is a compile error rather than a silent empty list.
//!
//! It lives in the protocol crate, like the request contract
//! ([`crate::request`]), because the client speaks it too: a typed client
//! dispatches a `PUSH` frame's op ([`crate::op::base::PUSH`]) to its
//! event type without linking the server engine.
//!
//! One op space: an event's op and a request's op ([`Request::OP`]) share
//! the game band, and `gsb-build` refuses a number both take.
//!
//! [`Request::OP`]: crate::request::Request::OP

use std::fmt::Debug;

use crate::request::WireMessage;

/// Where an event goes: the `.proto` declaration's `to:`
/// (`gsb.Destination`, without its refused zero value).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Dest {
    /// The receiver's room logic takes it (a balance, quest progress):
    /// the WORLD leg only.
    World,
    /// Straight to the receiver's client; no room ever sees it (chat,
    /// presence): the CLIENT leg only.
    Client,
    /// Both legs, each numbered, delivered and counted on its own.
    Both,
}

impl Dest {
    /// Does the WORLD leg run (`World` or `Both`)?
    pub const fn world(self) -> bool {
        matches!(self, Self::World | Self::Both)
    }

    /// Does the CLIENT leg run (`Client` or `Both`)?
    pub const fn client(self) -> bool {
        matches!(self, Self::Client | Self::Both)
    }
}

/// An event type. `gsb-build` writes the impl for every `.proto` message
/// with `(gsb.event)`, together with [`WorldEvent`] and/or
/// [`ClientEvent`] as [`Self::TO`] says.
pub trait Event: WireMessage + Clone + Debug {
    /// The event's op (game band, `>=`
    /// [`GAME_BAND_START`](crate::op::GAME_BAND_START)): the `op` of the
    /// `PUSH` frame that carries it to a client. Never reused, never
    /// shared with a request; `gsb-build` refuses both.
    const OP: u16;
    /// Where it goes.
    const TO: Dest;
}

/// An event a room may take: `to: WORLD` or `to: BOTH`.
pub trait WorldEvent: Event {}

/// An event a client may listen for: `to: CLIENT` or `to: BOTH`.
pub trait ClientEvent: Event {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_destination_runs_its_own_legs() {
        assert!(Dest::World.world() && !Dest::World.client());
        assert!(!Dest::Client.world() && Dest::Client.client());
        assert!(Dest::Both.world() && Dest::Both.client());
    }
}
