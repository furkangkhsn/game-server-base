//! The demux's half of connection migration (module `crate::udp::path`
//! has the rules and the reasons): a tagged datagram from a new address
//! starts a path validation, a matching response moves the session and
//! tells its writer and its actor (B113), and every validation ends
//! under one name. A child
//! of [`super`], so the demux's state stays private.

use std::net::SocketAddr;
use std::time::Instant;

use bytes::Bytes;
use gsb_core::conn::ConnIn;
use gsb_protocol::{FrameBody, op};
use tokio::sync::mpsc::error::TrySendError;
use tracing::debug;

use super::SessionKey;
use crate::udp::path::{CHALLENGE_LEN, PathProbe, draw_u64, encode_addr};
use crate::udp::*;

/// Connection migration's counters (each a metric, OPS §3). A
/// validation ends exactly once: `started = migrations + timed_out +
/// superseded + open_at_end` (plus the ones still pending).
#[derive(Debug, Default, Clone, Copy)]
pub(super) struct Counts {
    /// Sessions granted a CID.
    pub(super) cids_assigned: u64,
    /// CIDs or challenge nonces the entropy source could not supply.
    pub(super) entropy_failed: u64,
    /// Tagged datagrams naming no session: dropped.
    pub(super) cid_unknown: u64,
    pub(super) validations_started: u64,
    pub(super) challenges_sent: u64,
    pub(super) challenges_send_failed: u64,
    /// Challenge sends withheld by the amplification budget.
    pub(super) amplification_capped: u64,
    /// Candidates refused: the address is another session's.
    pub(super) address_in_use: u64,
    /// Path responses that answered no pending validation.
    pub(super) responses_unmatched: u64,
    /// Matching responses whose notices the writer's or the actor's
    /// channel refused — both are told or neither (the validation stays
    /// pending).
    pub(super) changes_not_forwarded: u64,
    pub(super) validations_timed_out: u64,
    pub(super) validations_superseded: u64,
    pub(super) validations_open_at_end: u64,
    /// Sessions moved to a validated address, and of those the moves
    /// that changed the port only (a NAT rebinding: the path estimate
    /// is kept).
    pub(super) migrations: u64,
    pub(super) migrations_port_only: u64,
}

impl Counts {
    /// A session ends with `p` pending: past its time it timed out,
    /// otherwise it was cut off.
    pub(super) fn validation_over(&mut self, p: &PathProbe, now: Instant) {
        match p.timed_out(now) {
            true => self.validations_timed_out += 1,
            false => self.validations_open_at_end += 1,
        }
    }
}

impl super::Demux {
    /// A pending validation past its time ends (lazily: on the session's
    /// next datagram, whatever its source).
    pub(super) fn path_expiry(&mut self, key: SessionKey, now: Instant) {
        let Some(s) = self.sessions.get_mut(key) else {
            return;
        };
        if s.path.is_some_and(|p| p.timed_out(now)) {
            s.path = None;
            self.mig.validations_timed_out += 1;
        }
    }

    /// A tagged datagram of `bytes` for `key` from `from`, not the
    /// session's address: start (or continue) validating `from`, and
    /// challenge it when one is due and the budget allows.
    pub(super) fn path_candidate(
        &mut self,
        key: SessionKey,
        from: SocketAddr,
        bytes: usize,
        now: Instant,
    ) {
        let pending = self.sessions.get(key).and_then(|s| s.path);
        match pending {
            Some(p) if p.addr == from => {
                if let Some(p) = self.sessions.get_mut(key).and_then(|s| s.path.as_mut()) {
                    p.heard(bytes);
                }
            }
            _ if self.sessions.addr_taken_by_other(&from, key) => {
                self.mig.address_in_use += 1;
                return;
            }
            _ => {
                let Some(nonce) = draw_u64() else {
                    self.mig.entropy_failed += 1;
                    return;
                };
                if pending.is_some() {
                    // The newest candidate wins (module `crate::udp::path`).
                    self.mig.validations_superseded += 1;
                }
                self.mig.validations_started += 1;
                if let Some(s) = self.sessions.get_mut(key) {
                    s.path = Some(PathProbe::new(from, nonce, bytes, now));
                }
            }
        }
        self.challenge(key, now);
    }

    /// Send the session's challenge if one is due and affordable. On a
    /// sealed door it is a sealed record (its size counts against the
    /// budget), and the session's writer seals and sends it (module
    /// `record`).
    pub(super) fn challenge(&mut self, key: SessionKey, now: Instant) {
        let sealed = self.seal.is_some();
        let bytes = match sealed {
            true => CHALLENGE_LEN + crate::seal::wire::OVERHEAD_S2C,
            false => CHALLENGE_LEN,
        };
        let Some(p) = self.sessions.get(key).and_then(|s| s.path) else {
            return;
        };
        if !p.challenge_due(now) {
            return;
        }
        if !p.affordable(bytes) {
            self.mig.amplification_capped += 1;
            return;
        }
        let challenge = encode_path_challenge(p.nonce);
        let taken = match sealed {
            true => self.queue_send(key, Some(p.addr), &challenge),
            false => self.sock.try_send_to(&challenge, p.addr).is_ok(),
        };
        if let Some(p) = self.sessions.get_mut(key).and_then(|s| s.path.as_mut()) {
            p.challenged(bytes, taken, now);
        }
        match (taken, sealed) {
            (true, _) => self.mig.challenges_sent += 1,
            (false, true) => self.seal_counts().challenges_not_queued += 1,
            (false, false) => self.mig.challenges_send_failed += 1,
        }
    }

    /// A path response for `key` from `from`: a match migrates the
    /// session (its writer and its actor told first — no notices, no
    /// move).
    pub(super) fn path_response(&mut self, key: SessionKey, from: SocketAddr, nonce: u64) {
        let Some(s) = self.sessions.get(key) else {
            return;
        };
        let Some(p) = s.path.filter(|p| p.answered_by(from, nonce)) else {
            self.mig.responses_unmatched += 1;
            return;
        };
        if self.sessions.addr_taken_by_other(&from, key) {
            // Taken since the validation began (a handshake from it):
            // the validation stays pending until it times out.
            self.mig.address_in_use += 1;
            return;
        }
        let old = s.addr;
        let notice = FrameBody::new(op::base::UDP_PATH, Bytes::from(encode_addr(p.addr)));
        // The writer (where replies go) and the actor (its `peer`, the
        // registry's per-source count — B113) are told together or not
        // at all: a slot in each channel first, then both notices.
        let told = match (s.out_tx.try_reserve(), s.in_tx.try_reserve()) {
            (Ok(writer), Ok(actor)) => {
                writer.send(vec![notice]);
                actor.send(ConnIn::PeerChanged { peer: from });
                Ok(())
            }
            (Err(TrySendError::Closed(())), _) | (_, Err(TrySendError::Closed(()))) => Err(true),
            _ => Err(false),
        };
        if let Err(gone) = told {
            // Full: the validation stays pending and the next response
            // retries. Closed: the session's actor or writer is gone.
            self.mig.changes_not_forwarded += 1;
            if gone {
                self.removed_actor_gone += 1;
                self.remove_session(key);
            }
            return;
        }
        self.sessions.move_to(key, from);
        // A pending session's place in the per-source cap follows (B89).
        self.per_source.moved(key, from.ip());
        if let Some(s) = self.sessions.get_mut(key) {
            s.path = None;
        }
        self.mig.migrations += 1;
        if old.ip() == from.ip() {
            self.mig.migrations_port_only += 1;
        }
        debug!(%old, new = %from, "rUDP: session migrated (path validated)");
    }
}
