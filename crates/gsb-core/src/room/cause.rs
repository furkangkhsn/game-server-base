//! Why a membership reached the disconnect path (BACKLOG F27,
//! `docs/RECONNECT.md` §3.3): the actor's own knowledge of which of its
//! call sites asked the policy, handed to the logic beside the player —
//! and, for a closed connection, the server verdict that closed it, as
//! the registry relayed it (BACKLOG F28).
//!
//! ```text
//! RoomControl::Detach{,By} / ShardMsg::Detach{,By} ─┐
//! input-idle ceiling (phase 0d)                    ─┼─▶ detach_player(.., cause)
//! TickCtx::kick (phase 3b/3c)                      ─┘      └─▶ on_disconnect_with(.., cause)
//!                                                                 └─(default)─▶ on_disconnect
//! ```
//!
//! The verdict's road (F28): the connection actor's end
//! (`RegistryMsg::ConnClosed::verdict`) → the registry → the
//! connection's dispatcher (`Close`) or the dispatcher-less detach →
//! `RoomControl::DetachBy` / `ShardMsg::DetachBy` →
//! [`DisconnectCause::ConnectionClosedBy`].

use crate::conn::ServerClose;

/// Why the room or shard actor is asking the disconnect policy
/// ([`GameLogic::on_disconnect_with`](crate::room::GameLogic::on_disconnect_with))
/// about a member. Named after what the ENGINE distinguishes at the
/// call site, not after gameplay: which one a game treats as "rage
/// quit", "AFK" or "banned" is the game's reading.
///
/// `#[non_exhaustive]`: the engine may learn to tell more ends apart
/// without breaking a logic that matches on it — a logic keeps a
/// wildcard arm for whatever it does not tell apart. It did once:
/// [`Self::ConnectionClosedBy`] (BACKLOG F28) refines
/// [`Self::ConnectionClosed`] with the server verdict that closed the
/// connection. A logic that matched `ConnectionClosed` to mean "any
/// closed connection" folds the refinement back with
/// [`Self::coarse`]; the kit's per-cause policy does exactly that
/// (an override for `ConnectionClosed` still answers every closed
/// connection that has no override of its own).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum DisconnectCause {
    /// The member's connection closed and no server verdict closed it —
    /// the registry's `ConnClosed` route (`RoomControl::Detach`,
    /// `ShardMsg::Detach`): the peer went away (an EOF, a reset, a
    /// WebSocket close, a failed write). Since F28 a close the SERVER
    /// decided reaches the policy as [`Self::ConnectionClosedBy`]
    /// instead. Still this cause where the engine cannot tell: a live
    /// session a newer one of its identity takes over (BACKLOG F32: the
    /// reconnect outran the old close, or the old socket is half-open
    /// and the registry closes it) reaches the policy right before the
    /// newer session's resume, before any verdict is known; and a
    /// dispatcher whose op queue was too full to take the close (B61)
    /// detaches without one. The room's shutdown never reaches it.
    ConnectionClosed,
    /// The input-idle ceiling
    /// ([`RoomConfig::max_idle_input_secs`](crate::room::RoomConfig::max_idle_input_secs),
    /// RECONNECT §16): the transport is alive but the member stopped
    /// playing. The same cause under both
    /// [`AfkAction`](crate::room::AfkAction)s — the action decides the
    /// connection's fate after the policy answered, not the entity's.
    IdleInput,
    /// The game's own kick ([`TickCtx::kick`](crate::room::TickCtx::kick),
    /// BACKLOG E8, RECONNECT §16.3), applied after the hooks that could
    /// ask have returned.
    Kicked,
    /// The member's connection closed on a SERVER verdict (BACKLOG F28)
    /// — a refinement of [`Self::ConnectionClosed`], which it was before
    /// F28. The verdict is the one the connection booked in
    /// `server_closes`: a transport guardrail
    /// ([`ServerClose::IdleTimeout`], [`ServerClose::WriteStall`],
    /// [`ServerClose::RelDead`]), the protocol-violation budget
    /// ([`ServerClose::ViolationBudget`]), a newer session's takeover
    /// ([`ServerClose::Superseded`]), … — or a verdict of ANOTHER
    /// membership of the same connection (BACKLOG B43: a kick or idle
    /// close that lands after the connection rejoined ends the NEW
    /// membership with `ConnectionClosedBy(Kicked)` /
    /// `ConnectionClosedBy(IdleInput)` — this room did not judge it,
    /// unlike [`Self::Kicked`] / [`Self::IdleInput`]).
    ConnectionClosedBy(ServerClose),
}

impl DisconnectCause {
    /// A closed connection's cause: [`Self::ConnectionClosedBy`] the
    /// `verdict`, or [`Self::ConnectionClosed`] when there is none.
    pub const fn closed(verdict: Option<ServerClose>) -> Self {
        match verdict {
            Some(v) => Self::ConnectionClosedBy(v),
            None => Self::ConnectionClosed,
        }
    }

    /// The cause without its detail: [`Self::ConnectionClosedBy`] folds
    /// to [`Self::ConnectionClosed`] (the cause it refines); every other
    /// cause is its own.
    pub const fn coarse(self) -> Self {
        match self {
            Self::ConnectionClosedBy(_) => Self::ConnectionClosed,
            other => other,
        }
    }

    /// The server verdict behind a closed connection, if one closed it.
    pub const fn verdict(self) -> Option<ServerClose> {
        match self {
            Self::ConnectionClosedBy(v) => Some(v),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests;
