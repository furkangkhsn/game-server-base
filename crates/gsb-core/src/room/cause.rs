//! Why a membership reached the disconnect path (BACKLOG F27,
//! `docs/RECONNECT.md` §3.3): the actor's own knowledge of which of its
//! call sites asked the policy, handed to the logic beside the player.
//!
//! ```text
//! RoomControl::Detach / ShardMsg::Detach ─┐
//! input-idle ceiling (phase 0d)          ─┼─▶ detach_player(.., cause)
//! TickCtx::kick (phase 3b/3c)            ─┘      └─▶ on_disconnect_with(.., cause)
//!                                                       └─(default)─▶ on_disconnect
//! ```

/// Why the room or shard actor is asking the disconnect policy
/// ([`GameLogic::on_disconnect_with`](crate::room::GameLogic::on_disconnect_with))
/// about a member. Named after what the ENGINE distinguishes at the
/// call site, not after gameplay: which one a game treats as "rage
/// quit", "AFK" or "banned" is the game's reading.
///
/// `#[non_exhaustive]`: the engine may learn to tell more ends apart
/// (the close verdict behind a [`Self::ConnectionClosed`], say) without
/// breaking a logic that matches on it — a logic keeps a wildcard arm
/// for whatever it does not tell apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum DisconnectCause {
    /// The member's connection closed — the registry's `ConnClosed`
    /// route (`RoomControl::Detach`, `ShardMsg::Detach`). The room does
    /// not learn why: the peer went away, a transport guardrail
    /// (idle timeout, write stall, a dead rUDP band, the violation
    /// budget) closed it, or a server verdict judged the connection
    /// while it was a member of ANOTHER membership (BACKLOG B43: a
    /// kick or idle close that lands after the connection rejoined
    /// ends the NEW membership with this cause — this room did not
    /// judge it). Two ends never reach the policy at all: a newer
    /// session superseding a live one (a leave, `on_leave`) and the
    /// room's shutdown.
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
}

#[cfg(test)]
mod tests;
