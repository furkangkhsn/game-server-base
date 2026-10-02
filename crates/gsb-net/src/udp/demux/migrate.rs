//! The demux's half of connection migration (module `crate::udp::path`
//! has the rules and the reasons): a tagged datagram from a new address
//! starts a path validation, a matching response moves the session and
//! tells its writer, and every validation ends under one name. A child
//! of [`super`], so the demux's state stays private.

use std::net::SocketAddr;
use std::time::Instant;

use bytes::Bytes;
use gsb_protocol::{FrameBody, op};
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
    /// Matching responses whose writer notice its channel refused (the
    /// validation stays pending).
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

    /// Send the session's challenge if one is due and affordable.
    pub(super) fn challenge(&mut self, key: SessionKey, now: Instant) {
        let Some(p) = self.sessions.get_mut(key).and_then(|s| s.path.as_mut()) else {
            return;
        };
        if !p.challenge_due(now) {
            return;
        }
        if !p.affordable(CHALLENGE_LEN) {
            self.mig.amplification_capped += 1;
            return;
        }
        let taken = self
            .sock
            .try_send_to(&encode_path_challenge(p.nonce), p.addr)
            .is_ok();
        p.challenged(CHALLENGE_LEN, taken, now);
        match taken {
            true => self.mig.challenges_sent += 1,
            false => self.mig.challenges_send_failed += 1,
        }
    }

    /// A path response for `key` from `from`: a match migrates the
    /// session (its writer told first — no notice, no move).
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
        match s.out_tx.try_send(vec![notice]) {
            Ok(()) => {}
            Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                self.mig.changes_not_forwarded += 1;
                return;
            }
            Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
                self.mig.changes_not_forwarded += 1;
                self.removed_actor_gone += 1;
                self.remove_session(key);
                return;
            }
        }
        self.sessions.move_to(key, from);
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
