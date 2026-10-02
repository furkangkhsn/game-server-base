//! One established session's transport state, as the demux keeps it
//! (`table` indexes it). A child of [`super`]: its fields are the
//! demux's, and no one else's.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::time::Instant;

use gsb_core::channel::{FrameBatch, Mailbox};
use gsb_core::conn::ConnIn;

/// One established session's transport state (all of it local to the
/// demux task — the demux is the actor; its map is its counter).
#[derive(Debug)]
pub(in crate::udp) struct UdpSession {
    /// The client's current (validated) address: where the demux's
    /// replies go, and the address index's entry (`table`).
    pub(super) addr: SocketAddr,
    /// The connection id the server granted (module `crate::udp::path`;
    /// `None`: migration off, or the client did not ask).
    pub(super) cid: Option<u64>,
    /// The pending path validation, if any (module `crate::udp::path`).
    pub(super) path: Option<crate::udp::path::PathProbe>,
    pub(super) in_tx: Mailbox<ConnIn>,
    /// Kept for ACK piggyback (the demux hands inbound ACKs to the
    /// session's writer through the outbound channel).
    pub(super) out_tx: Mailbox<FrameBatch>,
    pub(super) last_seen: Instant,
    /// Inbound reliable state (client→server): the next expected seq,
    /// plus the small out-of-order window awaiting the gap.
    pub(super) in_expected: u32,
    pub(super) in_oob: HashMap<u32, Vec<u8>>,
    pub(super) oob_dropped: u64,
    pub(super) dup_in: u64,
    /// Inbound frames dropped on a full (bounded) session mailbox.
    pub(super) inbox_full: u64,
    pub(super) inbox_full_warned: bool,
    /// The record layer's receive half on a sealed door (B5a; `None` on a
    /// plaintext one).
    pub(super) seal: Option<SessionSeal>,
}

/// A sealed session's receive state: its `Opener`, and the accept
/// datagram kept for a re-sent proof until the session's first record
/// opens (the client holds the session then, and never re-sends).
pub(in crate::udp) struct SessionSeal {
    pub(super) opener: crate::seal::Opener,
    pub(super) accept: Option<Box<[u8]>>,
}

/// No key material: the counters only.
impl std::fmt::Debug for SessionSeal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionSeal")
            .field("forged", &self.opener.forged())
            .field("accept_kept", &self.accept.is_some())
            .finish()
    }
}

impl UdpSession {
    /// A session just established at `addr` (no frame received yet: the
    /// client's first REL is seq 1).
    pub(super) fn new(
        addr: SocketAddr,
        cid: Option<u64>,
        in_tx: Mailbox<ConnIn>,
        out_tx: Mailbox<FrameBatch>,
        now: Instant,
    ) -> Self {
        Self {
            addr,
            cid,
            path: None,
            in_tx,
            out_tx,
            last_seen: now,
            in_expected: 1,
            in_oob: HashMap::new(),
            oob_dropped: 0,
            dup_in: 0,
            inbox_full: 0,
            inbox_full_warned: false,
            seal: None,
        }
    }
}
