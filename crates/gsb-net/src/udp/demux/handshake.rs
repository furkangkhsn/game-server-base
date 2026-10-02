//! The server half of the stateless cookie handshake: no state is
//! allocated before the peer proves it can receive at its own address.

use std::net::SocketAddr;
use std::time::Instant;

use gsb_core::channel::{FrameBatch, channel};
use gsb_core::conn::ConnIn;
use tracing::{debug, warn};

use super::SessionSeal;
use crate::transport::Endpoint;
use crate::udp::writer::{Link, udp_link_spawner};
use crate::udp::*;

impl super::Demux {
    pub(super) fn handle_hello(&mut self, n: usize, peer: SocketAddr) {
        let nonce = u64::from_le_bytes(self.buf[1..9].try_into().unwrap());
        let cookie = u64::from_le_bytes(self.buf[9..17].try_into().unwrap());
        // The cookie's time term, read from the clock HERE (not stored,
        // not ticked by anyone): the challenge is minted for the current
        // slot, and a proof is accepted for the current slot or the
        // previous one. That is what makes a captured proof expire.
        let slot = self.clock.slot();
        // An established peer never re-handshakes (its session is at
        // this address; a NAT rebind is a new address — migrated by CID
        // when it has one, module `crate::udp::path`). Its valid proof
        // again is a RE-SEND — its first accept, or the proof's first
        // copy, was lost — so it is answered with the session's accept
        // again (its CID included) and nothing else: no second session,
        // no second `ConnectionId`, no second CID. Anything else from it
        // (a challenge request, a proof that does not verify) is
        // ignored, as before.
        if self.sessions.at(&peer).is_some() {
            if cookie != 0 && self.cookie.verify(nonce, peer, cookie, slot) {
                self.reanswer(peer);
            }
            return;
        }
        if cookie == 0 {
            // Challenge request: answer with the proof (stateless — no
            // state allocated before the proof; the response is the same
            // size as the request, so forged traffic cannot amplify).
            self.challenges += 1;
            let hello = encode_hello(nonce, self.cookie.compute(nonce, peer, slot));
            if let Err(e) = self.sock.try_send_to(&hello, peer) {
                self.challenges_send_failed += 1;
                debug!(%peer, %e, "rUDP: challenge send failed");
            }
        } else if !self.cookie.verify(nonce, peer, cookie, slot) {
            // Forged, or issued more than one rotation ago (a replay of a
            // captured proof): drop, count, answer nothing.
            self.bad_cookie += 1;
        } else if !self.proof_admissible(n) {
            // A sealed door's proof without a well-formed Noise message 1
            // (module `noise`): refused and counted there, nothing
            // created, before it can take a per-source place.
        } else if !self.per_source.admits(peer.ip()) {
            // The source holds its cap of pending sessions (B89, module
            // `source`): checked only now, past the cookie, so only a
            // return-routable source is ever counted or refused. Nothing
            // is created and no accept goes out; the client's re-sent
            // proof gets in once a place is free.
        } else {
            self.establish(n, peer, nonce, cookie);
        }
    }

    /// A verified, admitted proof of `n` bytes from `peer`: establish the
    /// session — on a sealed door only once the budget, the CID and the
    /// Diffie-Hellman (module `noise`) produced its record halves.
    fn establish(&mut self, n: usize, peer: SocketAddr, nonce: u64, cookie: u64) {
        let noised = match self.seal.is_some() {
            true => match self.noise(n, nonce, cookie) {
                Some(x) => Some(x),
                None => return, // refused, counted in `noise`
            },
            false => None,
        };
        {
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
            // The capability byte after the proof (module
            // `crate::udp::path`): a CID only when this server grants
            // them and the client asked.
            let caps = if n > 18 { self.buf[18] } else { 0 };
            // A sealed session's CID came with its handshake (always: it
            // routes every c→s record); a plaintext one's as before.
            let (cid, sealer, seal) = match noised {
                Some(x) => (
                    Some(x.cid),
                    Some(x.sealer),
                    Some(SessionSeal {
                        opener: x.opener,
                        accept: Some(x.accept),
                    }),
                ),
                None => (
                    (self.migration && caps & CAP_CID != 0)
                        .then(|| self.grant_cid())
                        .flatten(),
                    None,
                    None,
                ),
            };
            let accept = match &seal {
                Some(s) => s.accept.as_deref().map(<[u8]>::to_vec),
                None => None,
            }
            .unwrap_or_else(|| encode_accept(1, cid));
            let mut session = UdpSession::new(peer, cid, in_tx, out_tx.clone(), now);
            session.seal = seal;
            let key = self.sessions.insert(session);
            if let Some(idle) = self.idle {
                self.deadlines.insert((now + idle, key));
            }
            self.established += 1;
            debug!(%peer, "rUDP session established (handshake complete)");
            // The demux keeps the ORIGINAL `in_tx` (it is the session's
            // delivery path for the lifetime of the session); the
            // endpoint carries a clone for the composition root (the
            // registry's notification clone + the pump parameter, which
            // the UDP spawner ignores — there is no per-connection
            // reader).
            let endpoint = Endpoint::new(udp_link_spawner(Link {
                sock: self.sock.clone(),
                peer,
                max_datagram: self.max_datagram,
                reaper: self.reaper.session(key),
                metrics: self.metrics.clone(),
                congestion: self.congestion,
                sealer,
            }))
            .with_peer(peer)
            .with_inbox(endpoint_in_tx, in_rx)
            .with_outbox(out_tx, out_rx);
            let pending = self.per_source.claim(key, peer.ip());
            match self
                .end_tx
                .try_send(Queued::new(endpoint, self.metrics.clone(), pending))
            {
                Ok(()) => {
                    // The accept: the session's cumulative ACK, "send me
                    // seq 1". It is what the client waits for before it
                    // counts itself connected (module docs, "Handshake
                    // loss"); 5 bytes for an 18-byte proof that only the
                    // owner of this return path could produce (13 with
                    // the CID, for a 19-byte proof; on a sealed door 77,
                    // `ACK{1}` + Noise message 2, for a 67-byte proof).
                    self.send_raw_accept(peer, &accept);
                }
                Err(crossbeam_channel::TrySendError::Full(queued)) => {
                    // The accept loop is far behind (pathological burst):
                    // the session is torn down (the dropped endpoint
                    // carries the inbox; nothing leaks). No accept went
                    // out, so the client re-sends its proof, and a re-send
                    // that finds room establishes the session afresh.
                    self.endpoints_dropped += 1;
                    drop(queued.into_endpoint());
                    self.remove_session(key);
                    warn!(%peer, "rUDP: endpoint channel full; session dropped");
                }
                Err(crossbeam_channel::TrySendError::Disconnected(queued)) => {
                    // The accept side is gone (the listener dropped): this
                    // session cannot be adopted; torn down, no accept
                    // sent — counted (B74).
                    self.accept_gone += 1;
                    drop(queued.into_endpoint());
                    self.remove_session(key);
                }
            }
        }
    }

    /// A valid proof again from `peer`'s established session: the
    /// accept again. On a plaintext door `ACK{next}` (+ the CID); on a
    /// sealed one the STORED accept — the same message 2, no second DH —
    /// until the session's first record opened (then the client holds the
    /// session and the proof is a stale copy: ignored).
    fn reanswer(&mut self, peer: SocketAddr) {
        let Some(s) = self.sessions.at(&peer) else {
            return;
        };
        let accept = match &s.seal {
            Some(seal) => match &seal.accept {
                Some(stored) => stored.to_vec(),
                None => return,
            },
            None => encode_accept(s.in_expected, s.cid),
        };
        self.proofs_reanswered += 1;
        self.send_raw_accept(peer, &accept);
    }

    /// Put an accept datagram on the socket. Best-effort like every ACK.
    fn send_raw_accept(&mut self, peer: SocketAddr, accept: &[u8]) {
        if let Err(e) = self.sock.try_send_to(accept, peer) {
            self.acks_send_failed += 1;
            debug!(%peer, %e, "rUDP: accept send failed (best-effort)");
        }
    }

    /// A fresh CID for a new session: random (unpredictable, never
    /// `ConnectionId`'s sequence) and unique among the sessions. `None` —
    /// counted, the session simply not migratable — when the entropy
    /// source fails or (once in 2^64) the draw collides.
    fn grant_cid(&mut self) -> Option<u64> {
        match crate::udp::path::draw_u64() {
            Some(cid) if !self.sessions.cid_taken(cid) => {
                self.mig.cids_assigned += 1;
                Some(cid)
            }
            _ => {
                self.mig.entropy_failed += 1;
                None
            }
        }
    }
}
