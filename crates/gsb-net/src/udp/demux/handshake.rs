//! The server half of the stateless cookie handshake: no state is
//! allocated before the peer proves it can receive at its own address.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::time::Instant;

use gsb_core::channel::{FrameBatch, channel};
use gsb_core::conn::ConnIn;
use tracing::{debug, warn};

use crate::transport::Endpoint;
use crate::udp::*;

impl super::Demux {
    pub(super) fn handle_hello(&mut self, peer: SocketAddr) {
        let nonce = u64::from_le_bytes(self.buf[1..9].try_into().unwrap());
        let cookie = u64::from_le_bytes(self.buf[9..17].try_into().unwrap());
        // An established peer must not re-handshake: ignore (its session
        // is keyed by this address; NAT rebind means a NEW address).
        if self.sessions.contains_key(&peer) {
            return;
        }
        // The cookie's time term, read from the clock HERE (not stored,
        // not ticked by anyone): the challenge is minted for the current
        // slot, and a proof is accepted for the current slot or the
        // previous one. That is what makes a captured proof expire.
        let slot = self.clock.slot();
        if cookie == 0 {
            // Challenge request: answer with the proof (stateless — no
            // state allocated before the proof; the response is the same
            // size as the request, so forged traffic cannot amplify).
            self.challenges += 1;
            let hello = encode_hello(nonce, self.cookie.compute(nonce, peer, slot));
            if let Err(e) = self.sock.try_send_to(&hello, peer) {
                debug!(%peer, %e, "rUDP: challenge send failed");
            }
        } else if self.cookie.verify(nonce, peer, cookie, slot) {
            // Proof verified: establish the session. The mailboxes are
            // created HERE (not in the accept loop) so that every
            // post-INIT2 datagram — the client's AUTH included — is
            // routable before the accept loop has spawned the actor
            // (the inbox buffers the gap; the client's RTO >> it).
            let (in_tx, in_rx) = channel::<ConnIn>(self.inbox_cap);
            let (out_tx, out_rx) = channel::<FrameBatch>(self.outbox_cap);
            // Clone for the endpoint FIRST (the original goes into the
            // session struct below).
            let endpoint_in_tx = in_tx.clone();
            let now = Instant::now();
            self.sessions.insert(
                peer,
                UdpSession {
                    in_tx,
                    out_tx: out_tx.clone(),
                    last_seen: now,
                    in_expected: 1,
                    in_oob: HashMap::new(),
                    oob_dropped: 0,
                    dup_in: 0,
                    inbox_full: 0,
                    inbox_full_warned: false,
                },
            );
            if let Some(idle) = self.idle {
                self.deadlines.insert((now + idle, peer));
            }
            self.established += 1;
            debug!(%peer, "rUDP session established (handshake complete)");
            // The demux keeps the ORIGINAL `in_tx` (it is the session's
            // delivery path for the lifetime of the session); the
            // endpoint carries a clone for the composition root (the
            // registry's notification clone + the pump parameter, which
            // the UDP spawner ignores — there is no per-connection
            // reader).
            let endpoint =
                Endpoint::new(udp_pump_spawner(self.sock.clone(), peer, self.max_datagram))
                    .with_peer(peer)
                    .with_inbox(endpoint_in_tx, in_rx)
                    .with_outbox(out_tx, out_rx);
            match self.end_tx.try_send(endpoint) {
                Ok(()) => {}
                Err(crossbeam_channel::TrySendError::Full(_)) => {
                    // The accept loop is far behind (pathological burst):
                    // the session is torn down (the dropped endpoint
                    // carries the inbox; nothing leaks). The client's AUTH
                    // retransmits find no session; it re-handshakes after
                    // its timeout.
                    self.endpoints_dropped += 1;
                    self.remove_session(peer);
                    warn!(%peer, "rUDP: endpoint channel full; session dropped");
                }
                Err(crossbeam_channel::TrySendError::Disconnected(_)) => {
                    // The accept loop is gone (shutdown in flight): this
                    // session cannot be adopted; tear it down quietly.
                    self.remove_session(peer);
                }
            }
        } else {
            // Forged, or issued more than one rotation ago (a replay of a
            // captured proof): drop, count, answer nothing.
            self.bad_cookie += 1;
        }
    }
}
