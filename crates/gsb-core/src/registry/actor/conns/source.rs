//! The per-source cap on unauthenticated connections (BACKLOG D12,
//! SECURITY §4.3.1).
//!
//! Plain TCP has no handshake stage, so the doors' per-source handshake
//! cap (D11) never sees it: a TCP peer is unauthenticated from its first
//! byte, and one address could fill the whole unauthenticated pool
//! (`max_unauth_conns`) and get every other source refused. The other
//! doors' sessions reach the same pool once their handshake ends, and a
//! handshake is cheap to finish; so the cap is here, where that pool is
//! counted, and covers every door's sessions.
//!
//! - **What it counts.** Rows still unauthenticated whose
//!   [`Source`] is the new connection's — the D11 rule (an IPv4 address,
//!   an IPv6 /64, a mapped address as its IPv4 one), one rule for both
//!   stages. A row leaves the count when it authenticates (`Authed`) or
//!   goes (its close); a failed AUTH leaves the session unauthenticated,
//!   so it keeps its place until it succeeds or closes — like the pool.
//! - **No table of its own.** The count is a scan of the connection
//!   rows, fused with the pool's own scan (one pass for both caps), so
//!   nothing is to be given back and nothing can leak: every row that
//!   leaves the connection table, by whichever of its exits, leaves the
//!   count with it. The only state is one `Source` per row — bounded by
//!   the rows, the unauthenticated ones by `max_unauth_conns`.
//! - **No lock.** The registry actor owns the rows; it is the one task
//!   that decides a birth.

use std::fmt::Debug;
use std::hash::Hash;

use tracing::{debug, warn};

use crate::channel::Mailbox;
use crate::conn::{ConnIn, ServerClose};
use crate::id::ConnectionId;
use crate::registry::actor::Registry;
use crate::source::Source;

/// The unauthenticated rows a birth is decided over: all of them, and
/// those of the new connection's source.
pub(super) struct Unauthed {
    pub(super) total: u64,
    pub(super) from_source: u64,
}

impl<W, G, St, Sp> Registry<W, G, St, Sp>
where
    W: Send + 'static,
    G: Eq + Hash + Clone + Debug + Send + 'static,
    St: Debug + Send + 'static,
    Sp: Debug + Clone + PartialEq + Send + 'static,
{
    /// Cap one source's unauthenticated connections at `cap` (BACKLOG
    /// D12; `None`, the default, or `Some(0)` = no cap): a connection
    /// born while its source already holds `cap` of them is refused like
    /// the pool's own refusals — no row, `ERROR` code 9, then EOF — and
    /// counted as [`crate::conn::ServerClose::UnauthSourceCap`].
    pub fn with_unauth_per_source(mut self, cap: Option<u64>) -> Self {
        self.max_unauth_per_source = cap.filter(|&n| n > 0);
        self
    }

    /// Refuse `conn` when its source already holds its cap of
    /// unauthenticated rows (`unauthed`, counted for this birth): told
    /// like the pool's refusals, no row recorded. Returns whether it was.
    pub(super) fn refused_per_source(
        &mut self,
        conn: ConnectionId,
        inbox: &Mailbox<ConnIn>,
        source: Option<Source>,
        unauthed: &Unauthed,
    ) -> bool {
        let (Some(cap), Some(source)) = (self.max_unauth_per_source, source) else {
            return false;
        };
        if unauthed.from_source < cap {
            return false;
        }
        if !std::mem::replace(&mut self.source_cap_warned, true) {
            warn!(
                %conn,
                %source,
                cap,
                "unauthenticated connections from one source at the per-source cap; refusing its new ones"
            );
        }
        debug!(%conn, %source, "per-source unauthenticated cap reached");
        self.tell(
            conn,
            inbox,
            ConnIn::ServerClosed {
                cause: ServerClose::UnauthSourceCap,
                reason: "source at its per-source unauthenticated capacity".into(),
            },
        );
        true
    }

    /// Count the unauthenticated rows, all and `source`'s, in one pass —
    /// only when a cap reads them (an O(connections) scan on the open
    /// path, a control-plane-rate event).
    pub(super) fn unauthed(&self, source: Option<Source>) -> Unauthed {
        let mut n = Unauthed {
            total: 0,
            from_source: 0,
        };
        let per_source = self.max_unauth_per_source.and(source);
        if self.max_unauth_conns.is_none() && per_source.is_none() {
            return n;
        }
        for info in self.conns.values().filter(|i| !i.authed) {
            n.total += 1;
            if per_source.is_some() && info.source == per_source {
                n.from_source += 1;
            }
        }
        n
    }
}
