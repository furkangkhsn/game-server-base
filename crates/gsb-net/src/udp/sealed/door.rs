//! The demux's sealed-door state (B5a, B5b): the identity, the stateless
//! reset key and its budget, the DH budget, the key-phase policy its
//! writers get, and the counters. A child of [`super`].
//!
//! **The reset key (decision 9, B5b).** The server config's
//! `udp_reset_key` when set; otherwise derived from the static key
//! (`ResetKey::derived_from`) — the static key is required on a sealed
//! door and survives every restart (clients pin it), so resets survive
//! restarts with no second secret to manage, and the operator who wants
//! the two rotated apart sets one. Either way the key is bound to the
//! door's bound address (`ResetKey::for_door`): a door that does not
//! know a CID must not answer with the token of another door's live
//! session (a sniffer who read a CID on one door could otherwise ask the
//! other for the session's token).

use std::net::SocketAddr;
use std::sync::Arc;

use crate::seal::{ResetKey, StaticKey};
use crate::udp::UdpTransportConfig;
use crate::udp::path::encode_addr;

use super::{DEFAULT_STATELESS_RESETS_PER_SEC, DhBudget, RekeyPolicy};

/// The sealed door's own counters (each a metric, OPS §3; the budgets'
/// refusals are their `refused`).
#[derive(Debug, Default, Clone, Copy)]
pub(in crate::udp) struct Counts {
    pub(in crate::udp) proofs_refused_plaintext: u64,
    pub(in crate::udp) handshakes_malformed: u64,
    pub(in crate::udp) handshakes_failed_decrypt: u64,
    pub(in crate::udp) handshakes_failed_internal: u64,
    pub(in crate::udp) datagrams_unsealed: u64,
    /// One per `seal::Refusal`, in `Refusal::ALL` order.
    pub(in crate::udp) refused: [u64; 6],
    pub(in crate::udp) sessions_ended_limit: u64,
    pub(in crate::udp) candidates_not_newest: u64,
    pub(in crate::udp) acks_not_queued: u64,
    pub(in crate::udp) challenges_not_queued: u64,
    /// Stateless resets sent, and the ones the socket refused (B5b).
    pub(in crate::udp) resets_sent: u64,
    pub(in crate::udp) resets_send_failed: u64,
}

impl Counts {
    /// Count one refused record under its name.
    pub(in crate::udp) fn refusal(&mut self, r: crate::seal::Refusal) {
        let i = crate::seal::Refusal::ALL
            .iter()
            .position(|x| *x == r)
            .expect("every refusal is in ALL");
        self.refused[i] += 1;
    }
}

/// The demux's sealed-door state (module docs).
pub(in crate::udp) struct DoorSeal {
    pub(in crate::udp) key: Arc<StaticKey>,
    /// Derives each session's reset token (module docs).
    pub(in crate::udp) reset: ResetKey,
    /// The stateless reset budget (`None`: the door sends none).
    pub(in crate::udp) resets: Option<DhBudget>,
    pub(in crate::udp) budget: DhBudget,
    /// The key-phase policy of the door's writers (module `rekey`).
    pub(in crate::udp) rekey: RekeyPolicy,
    pub(in crate::udp) counts: Counts,
    /// A plaintext client's proof was logged once (the rest are counted).
    pub(in crate::udp) warned_plaintext: bool,
}

impl DoorSeal {
    /// A door under `key` with the DH budget `per_sec`, the reset key
    /// derived from `key` (not yet bound to a door), the default reset
    /// budget and key-phase policy. [`Self::configure`] gives it the
    /// door's own.
    pub(in crate::udp) fn new(key: Arc<StaticKey>, per_sec: Option<u32>) -> Self {
        Self {
            reset: ResetKey::derived_from(&key),
            key,
            resets: resets_budget(DEFAULT_STATELESS_RESETS_PER_SEC),
            budget: DhBudget::new(per_sec),
            rekey: RekeyPolicy::default(),
            counts: Counts::default(),
            warned_plaintext: false,
        }
    }

    /// The door's configuration: its reset key (the configured one, or
    /// the static key's derivation) bound to `door`, the reset budget,
    /// the key-phase policy.
    pub(in crate::udp) fn configure(mut self, cfg: &UdpTransportConfig, door: SocketAddr) -> Self {
        let door = encode_addr(door);
        self.reset = match &cfg.reset_key {
            Some(k) => k.for_door(&door),
            None => ResetKey::derived_from(&self.key).for_door(&door),
        };
        self.resets = resets_budget(cfg.stateless_resets_per_sec);
        self.rekey = cfg.rekey;
        self
    }
}

/// The reset budget for `per_sec` (`0`: no resets at all).
fn resets_budget(per_sec: u32) -> Option<DhBudget> {
    (per_sec > 0).then(|| DhBudget::new(Some(per_sec)))
}

impl std::fmt::Debug for DoorSeal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DoorSeal")
            .field("budget", &self.budget)
            .field("resets", &self.resets)
            .field("counts", &self.counts)
            .finish_non_exhaustive()
    }
}
