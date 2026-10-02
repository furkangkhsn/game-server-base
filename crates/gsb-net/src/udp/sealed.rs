//! The record layer, wired (BACKLOG B5a; `docs/RUDP-SECURITY.md`): the
//! door's security mode, the sealed handshake's wire, the global
//! Diffie-Hellman budget and the demux → writer send request. The crypto
//! itself is `crate::seal` (sans-IO); this module is where rUDP meets it.
//!
//! ## The two modes
//!
//! - **Sealed** ([`UdpSecurity::Sealed`], the server's production
//!   default — decision 6): the cookie handshake carries Noise NK (0
//!   extra RTT), and after it EVERY datagram of the session, both ways,
//!   is a SEALED record `[kind][cid c→s][counter][ciphertext][tag]` whose
//!   plaintext is the datagram the plaintext door would have sent (RAW,
//!   REL, ACK, FRAG, PROBE, REPORT, PATH_*), unchanged.
//! - **Plaintext** ([`UdpSecurity::Plaintext`]): the door as it was
//!   before B5a, byte for byte — the explicit dev/LAN switch.
//!
//! ## The sealed handshake (RUDP-SECURITY §4)
//!
//! ```text
//! client                                         server
//! HELLO{n,0}                        18 B ──▶
//!                                        ◀──     HELLO{n,cookie}  18 B   no state, no DH
//! proof [3][n][cookie][0][caps][msg1]    ──▶     cookie → msg1 shape → per-source cap
//!       19 B + msg1 48 B = 67 B                  → DH budget → CID → DH (msg1 → msg2)
//!                                        ◀──     accept [2][u32 1][msg2]  5 + 72 = 77 B
//! SEALED REL AUTH(ticket)                ──▶
//! ```
//!
//! - The Noise prologue's context is the HELLO nonce and cookie (16 B,
//!   [`context`]): a message 1 captured from another cookie exchange does
//!   not authenticate.
//! - msg2's encrypted payload is `seal::Accept`: the session's CID (the
//!   routing key of every c→s record; always granted on a sealed door)
//!   and its stateless reset token (B5b; module `door`): after a restart
//!   the door answers the CID it no longer knows with a reset carrying
//!   that token, and the client ends the session at once.
//! - **Idempotent proof:** the accept datagram is stored with the session
//!   until its first record opens; a re-sent proof from the session's
//!   address gets the same bytes — no second DH.
//! - A plaintext proof at a sealed door (no msg1) is refused and counted
//!   (`udp_proofs_refused_plaintext`): the old client's handshake times
//!   out. A sealed client at a plaintext door gets a plaintext accept, no
//!   msg2: it refuses it, counts it, and gives up at the deadline with
//!   `ConnectionRefused` (DESIGN §5 — the one deliberate compatibility
//!   break of the rUDP line).
//!
//! ## Two halves, no shared state
//!
//! A session's `Opener` lives in the demux (the one task that reads the
//! socket), its `Sealer` in the session's writer (the one task that sends
//! for it). What the demux itself must send to a sealed session — the
//! reliable band's cumulative ACK, a path challenge — goes to the writer
//! as a `UDP_SEND` request on the outbound channel (like the piggybacked
//! ACK), and the writer seals it: one counter space, one owner, no lock.

use std::net::SocketAddr;
use std::sync::Arc;

use crate::seal::{MSG1_LEN_MAX, MSG1_LEN_MIN, MSG2_LEN, StaticKey};
use crate::udp::path::{decode_addr, encode_addr};
use crate::udp::*;

mod budget;
pub(super) use budget::DhBudget;
mod door;
pub(super) use door::{Counts, DoorSeal};
mod rekey;
pub(super) use rekey::SendHalf;
pub use rekey::{DEFAULT_REKEY_AFTER, DEFAULT_REKEY_AFTER_RECORDS, RekeyPolicy};

/// The default of the door's handshake budget (B119): sealed handshakes
/// per second the demux runs its Diffie-Hellman for. Measured (B110,
/// u89): ~180 µs of one core per responder handshake on a loaded Ryzen
/// 9 7950X, so 1000/s holds the demux — the one task every session's
/// inbound traffic shares — at ~18 % of a core for key agreement, and
/// still admits a 1000-player join storm within about a second. `0`
/// (or `None`) = no budget.
pub const DEFAULT_HANDSHAKES_PER_SEC: u32 = 1000;

/// The default of the door's stateless reset budget (B5b): resets per
/// second the demux sends in answer to records with an unknown CID — a
/// token bucket holding 50 ms of it. Each costs one HMAC and one send
/// (~3 µs); 10 000/s is ~3 % of the demux's core and lets a restarted
/// server reset 10 000 sessions within about a second (each client gets
/// the next token its datagrams find). `0`: no resets.
pub const DEFAULT_STATELESS_RESETS_PER_SEC: u32 = 10_000;

/// The door's security mode (see the module docs).
#[derive(Clone, Default)]
pub enum UdpSecurity {
    /// No record layer: the door as it was before B5a (dev/LAN only).
    #[default]
    Plaintext,
    /// Noise NK handshake + SEALED records, under this static key (the
    /// server's identity; clients pin its public half).
    Sealed(Arc<StaticKey>),
}

impl UdpSecurity {
    /// The pinned public key clients need (`None` on a plaintext door).
    pub fn public_key(&self) -> Option<[u8; crate::seal::KEY_LEN]> {
        match self {
            UdpSecurity::Plaintext => None,
            UdpSecurity::Sealed(k) => Some(k.public()),
        }
    }
}

/// The public half at most: never the key.
impl std::fmt::Debug for UdpSecurity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            UdpSecurity::Plaintext => f.write_str("Plaintext"),
            UdpSecurity::Sealed(_) => f.write_str("Sealed(..)"),
        }
    }
}

/// Where a sealed proof's Noise message 1 starts: after the HELLO's 18
/// bytes and the capability byte (which a sealed proof always carries).
pub(super) const PROOF_MSG1_AT: usize = 19;
/// A sealed accept: `ACK{1}` (5 B) and message 2.
pub(super) const SEALED_ACCEPT_LEN: usize = 5 + MSG2_LEN;
/// The shortest and longest sealed proof.
pub(super) const SEALED_PROOF_MIN: usize = PROOF_MSG1_AT + MSG1_LEN_MIN;
pub(super) const SEALED_PROOF_MAX: usize = PROOF_MSG1_AT + MSG1_LEN_MAX;

/// The Noise prologue context of one cookie exchange: the HELLO nonce
/// and the cookie, little-endian (RUDP-SECURITY §4).
pub(super) fn context(nonce: u64, cookie: u64) -> [u8; 16] {
    let mut c = [0u8; 16];
    c[..8].copy_from_slice(&nonce.to_le_bytes());
    c[8..].copy_from_slice(&cookie.to_le_bytes());
    c
}

/// A sealed proof: the HELLO, the capability byte (always present, so
/// message 1 sits at a fixed offset) and message 1.
pub(super) fn encode_sealed_proof(nonce: u64, cookie: u64, caps: u8, msg1: &[u8]) -> Vec<u8> {
    let mut d = encode_hello(nonce, cookie);
    d.push(caps);
    d.extend_from_slice(msg1);
    d
}

/// A sealed accept: `ACK{1}` and message 2 (the client's next REL is
/// seq 1, as on the plaintext door).
pub(super) fn encode_sealed_accept(msg2: &[u8]) -> Vec<u8> {
    let mut d = encode_ack(1);
    d.extend_from_slice(msg2);
    d
}

/// A `UDP_SEND` request's payload (demux → writer, never on the wire):
/// `0` for the session's current address or the `UDP_PATH` address
/// encoding, then the inner datagram the writer seals.
pub(super) fn encode_send(to: Option<SocketAddr>, inner: &[u8]) -> Vec<u8> {
    let mut v = match to {
        None => vec![0],
        Some(a) => encode_addr(a),
    };
    v.extend_from_slice(inner);
    v
}

/// The inverse of [`encode_send`]: the destination and the inner
/// datagram (never empty).
pub(super) fn decode_send(b: &[u8]) -> Option<(Option<SocketAddr>, &[u8])> {
    let (to, at) = match *b.first()? {
        0 => (None, 1),
        4 => (Some(decode_addr(b)?), 1 + 4 + 2),
        6 => (Some(decode_addr(b)?), 1 + 16 + 2),
        _ => return None,
    };
    let inner = b.get(at..).filter(|i| !i.is_empty())?;
    Some((to, inner))
}

#[cfg(test)]
mod tests;
