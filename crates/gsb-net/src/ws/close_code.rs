//! The status code of the server's teardown close, by how the session
//! ended (BACKLOG B30; the code itself since B24).
//!
//! Until B30 the door did not know why the session ended — it saw only
//! the outbound channel close — so every teardown close said 1001 "Going
//! Away", a stop's and a cheater's alike. The connection actor now tells
//! the door ([`SessionEnd`], `gsb_core::conn`), and the code follows it.
//! A client that sees only the close (a browser's `CloseEvent`, a proxy
//! log) can then tell "the server went away — reconnect" from "you were
//! closed for what you did — do not just reconnect" from "the server is
//! full — try later". The `ERROR` frame ahead of the close is unchanged;
//! it carries the precise reason for a client that reads it.
//!
//! The mapping (RFC 6455 §7.4.1, and the IANA WebSocket Close Code
//! Number Registry for 1013):
//!
//! | how it ended | code | why this code |
//! |---|---|---|
//! | the server's stop | 1001 Going Away | "a server going down" — the RFC's own example; `ERROR` 14 says the same |
//! | no verdict (the client ended it), or never told | 1001 | unchanged; a client that closed does not read it (its own close handshake's echo wins) |
//! | `room_gone` | 1001 | the endpoint the session lived in went away — the client may join elsewhere at once |
//! | `outbound_dead` | 1001 | no verdict on the session, only a dead outbound path (the close rarely lands at all) |
//! | `idle_timeout`, `write_stall`, `rel_dead` | 1008 Policy Violation | the server's liveness policy ended THIS session: the peer stopped sending or reading |
//! | `violation_budget`, `preauth_budget`, `stream_rejected` | 1008 | the peer broke the protocol policy (the reader's own 1002/1003/1007/1009, queued first, wins for a refused stream) |
//! | `idle_input`, `kicked` | 1008 | the room's / the game's policy ended the membership and the session |
//! | `superseded` | 1008 | the "latest session wins" policy — a client must NOT reconnect blindly (it would take the newer session over in turn) |
//! | `conn_cap`, `unauth_cap` | 1013 Try Again Later | the server is at capacity: the session did nothing wrong, a later attempt may succeed |
//! | `unauth_source_cap` | 1013 | the session's source holds its share of unauthenticated sessions (D12): one of them authenticating or leaving frees a place |
//!
//! 1011 (Internal Error) is not used: no verdict means "the server
//! failed" — a room that died under the session is `room_gone`, which
//! does not tell a panic from a destroy. 1000 (Normal Closure) is not
//! used either: every teardown here is the server ending a session the
//! client did not ask to end.

use gsb_core::conn::{ServerClose, SessionEnd};

/// RFC 6455 §7.4.1 status 1001 "Going Away" (B24).
pub(super) const CLOSE_GOING_AWAY: u16 = 1001;

/// RFC 6455 §7.4.1 status 1008 "Policy Violation".
pub(super) const CLOSE_POLICY_VIOLATION: u16 = 1008;

/// Status 1013 "Try Again Later" (IANA WebSocket Close Code Number
/// Registry).
pub(super) const CLOSE_TRY_AGAIN_LATER: u16 = 1013;

/// The teardown close's status code for `end` (`None`: the actor never
/// told — 1001, as before B30). See the module docs for the mapping.
pub(super) fn close_code(end: Option<SessionEnd>) -> u16 {
    let verdict = match end {
        None | Some(SessionEnd::Client) | Some(SessionEnd::Stopped) => return CLOSE_GOING_AWAY,
        Some(SessionEnd::Verdict(verdict)) => verdict,
    };
    match verdict {
        ServerClose::RoomGone | ServerClose::OutboundDead => CLOSE_GOING_AWAY,
        ServerClose::IdleTimeout
        | ServerClose::WriteStall
        | ServerClose::RelDead
        | ServerClose::ViolationBudget
        | ServerClose::PreauthBudget
        | ServerClose::StreamRejected
        | ServerClose::Superseded
        | ServerClose::IdleInput
        | ServerClose::Kicked => CLOSE_POLICY_VIOLATION,
        ServerClose::ConnCap | ServerClose::UnauthCap | ServerClose::UnauthSourceCap => {
            CLOSE_TRY_AGAIN_LATER
        }
    }
}

#[cfg(test)]
mod tests;
