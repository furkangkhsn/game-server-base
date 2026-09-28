//! The live session a resume takes over (BACKLOG F32,
//! `docs/RECONNECT.md` §5 "Çift oturum"): an identified resume that finds
//! the identity still bound to a LIVE row of another connection.
//!
//! Two orders put it there, both a reconnect that outran the server's
//! notice of the old transport's end:
//!
//! - the old connection's `Detach` is still on its way (its dispatcher
//!   sends it; the new connection's dispatcher sends the resume — two
//!   tasks, no order between them);
//! - the registry saw the new join before the old connection's close
//!   (the old socket is half-open, or its close is still queued): it
//!   closes the old socket with ERROR 9 and hands the membership over
//!   instead of leaving it.
//!
//! Either way the room runs the old session's detach HERE, first — the
//! policy the late `Detach` would have run — and the resume then finds
//! the park like any other. The old connection's own `Detach`, whenever
//! it lands, finds its binding moved and is a no-op.

use std::collections::HashMap;

use crate::id::{ConnectionId, PlayerId};
use crate::room::RoomConn;

/// The member `identity` holds LIVE (not parked) under a connection other
/// than `conn`, with that connection: the session a resume by `conn`
/// takes over. `None` for an anonymous identity, for a parked row (the
/// resume's own target) and for `conn`'s own row (a rejoin of the same
/// connection supersedes its own state, as a join always has).
///
/// O(members) — a control-plane event, like the registry's supersedence
/// scan; at most one row matches (one identity, one member).
pub(crate) fn live_session<G>(
    conns: &HashMap<PlayerId, RoomConn<G>>,
    identity: &str,
    conn: ConnectionId,
) -> Option<(PlayerId, ConnectionId)> {
    if identity.is_empty() {
        return None;
    }
    conns
        .iter()
        .find(|(_, rc)| !rc.detached && rc.conn != conn && rc.identity == identity)
        .map(|(&player, rc)| (player, rc.conn))
}
