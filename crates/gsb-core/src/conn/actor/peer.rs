//! The session's client address moved (BACKLOG B113): an rUDP connection
//! migration, told by the transport after path validation
//! ([`ConnIn::PeerChanged`](crate::conn::ConnIn::PeerChanged)). The
//! actor's `peer` — what its close signals name — follows, and the
//! registry hears the new source, so its per-source unauthenticated
//! count (D12) can follow too. The notice shares the inbox with the
//! session's frames, so it is handled in order with them; its send to
//! the registry is the one `Authed` uses, so the two stay in order.

use std::net::SocketAddr;

use tracing::debug;

use crate::registry::RegistryMsg;
use crate::source::Source;

impl super::ConnectionActor {
    pub(super) async fn on_peer_changed(&mut self, peer: SocketAddr) {
        let old = std::mem::replace(&mut self.peer, peer);
        debug!(%self.conn, %old, new = %peer, "the session's client address moved");
        // A failed send means the registry is gone (shutdown) — ignored,
        // like every other notice to it.
        let _ = self
            .registry
            .send(RegistryMsg::ConnPeerChanged {
                conn: self.conn,
                source: Source::of(peer.ip()),
            })
            .await;
    }
}

#[cfg(test)]
mod tests;
