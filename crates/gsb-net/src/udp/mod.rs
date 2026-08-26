//! The rUDP transport: one socket for every session.
//!
//! ## The structural difference (why this module exists)
//!
//! TCP gives each connection its own socket: per-connection reader and
//! writer pumps, each owning its socket half. UDP gives **all** sessions
//! ONE socket. So:
//!
//! - [`UdpListener::accept`] does not accept a socket; it returns the
//!   next **session** the shared demux synthesizes from the datagram
//!   stream (a completed handshake from a new peer).
//! - the read path is a **single task** — the **demux** — that receives
//!   every datagram and routes it to the right session's mailbox.
//! - [`Endpoint::start_pump`] no longer spawns a per-connection reader:
//!   the UDP pump returns a `None` reader handle and only the per-session
//!   **writer** task (outbound batch → `send_to`). The demux is owned by
//!   the listener (it outlives every connection) and is stopped via
//!   [`Listener::close`].
//!
//! ## Handshake (anti-amplification)
//!
//! A datagram's source address is forgeable, so no session state may be
//! allocated — and no answer owed — before the peer proves it owns the
//! return path. The proof is a stateless cookie:
//!
//! ```text
//! client → HELLO { nonce, cookie: 0 }        (challenge request)
//! server → HELLO { nonce, cookie: F(nonce, peer, key) }
//! client → HELLO { nonce, cookie }           (proof)
//! server: cookie verified → session established (channels pre-created)
//! ```
//!
//! `F` is a per-process-keyed mix (splitmix64 folds of `nonce`, the peer
//! address, and the key) — not a KDF: v1 has no crypto layer (out of
//! scope), and the property needed is "unforgeable over the network
//! without the key". The challenge response is well-formed only for a
//! well-formed challenge (same 18-byte size), so forged-traffic
//! amplification stays at ratio ≤ 1, and a forged proof needs the
//! cookie, which needs the key.
//!
//! **The key** (16 bytes) is drawn from the **OS entropy source** at
//! bind time (the `getrandom(2)`/`BCryptGenRandom` reader — 128 bits of
//! uniform material; a 64-bit secret would already be enough) or
//! supplied by the operator through the config (`cookie_key`). It is
//! deliberately **not** a function of the wall clock: a clock-derived
//! key is computable by an off-path attacker who narrows the server's
//! *start time* to a small window, and a forged proof then never needs
//! the challenge — the handshake's reason to exist evaporates. If the
//! entropy source cannot be read and no config key is present, `bind`
//! **fails** (the server refuses to start): the key is the entire basis
//! of the property, so running with a predictable key would invert it
//! rather than weaken it, and a startup warning is not a security
//! posture (see [`CookieKey`]).
//!
//! `UDP_HELLO`/`UDP_ACK` opcodes live in the base band but are
//! **transport markers**: they travel in their own datagram kinds and are
//! handled below the actor layer (the connection actor never sees them;
//! they are not message-table messages).
//!
//! ## Datagram framing (both directions)
//!
//! ```text
//! [u8 kind]
//!   0 RAW    [u16 LE op][payload]               game band: loss-tolerant
//!   1 REL    [u32 LE seq][u16 LE op][payload]   control band: reliable
//!   2 ACK    [u32 LE next expected seq]         cumulative, per direction
//!   3 HELLO  [u64 LE nonce][u64 LE cookie]      handshake
//! ```
//!
//! The band split is by opcode: `op <= 64` (base control band) is
//! REL, `op >= 1000` (game band) is RAW.
//!
//! **Status: experimental (v1).** Validated on loopback and by the e2e
//! suite; NOT hardened for lossy real-world networks. The known gap:
//! the REL give-up (`RETRANSIT_MAX`) is silent — a frame abandoned
//! after 250 ms wedges its direction's cumulative stream (the receiver
//! never advances past the hole) while the session stays alive and the
//! RAW game band keeps flowing, so the wedge is invisible. Production
//! deployments should run a hardened transport behind the same
//! [`crate::Transport`] seam, or close this gap first (give-up ⇒
//! session-fatal, or memory-bounded retransmit with a no-ACK death
//! threshold — see ROADMAP "dış inceleme hızlı düzeltme turu", Kalan).
//!
//! ## Band semantics (feature 2)
//!
//! - **Control band (AUTH/JOIN/LEAVE/HEARTBEAT/ERROR): reliable within
//!   the give-up bound.** Loss would break correctness (a lost JOIN
//!   result hangs the client; a lost HEARTBEAT is tolerable only
//!   because HEARTBEAT_ACK is not state — but the *request* may be).
//!   Each direction keeps its own sequence: the sender retransmits the
//!   oldest un-ACKed REL frame every `RETRANSIT_RTO` until it is ACKed
//!   or `RETRANSIT_MAX` elapses (give-up, counted — and see the status
//!   note above: the count has NO consequence today, which is exactly
//!   why this transport is experimental). The receiver deduplicates
//!   (cumulative), buffers a small out-of-order window, and only
//!   advances when the gap fills — control frames are never delivered
//!   out of order (AUTH before JOIN is a property of `seq`, not of
//!   luck).
//! - **Snapshot band (WORLD_SNAPSHOT & co.): RAW.** The room's snapshots
//!   are self-contained (a client that loses one heals on the next
//!   snapshot or the keep-alive resend — `RoomConfig::keepalive_hz`), so
//!   reliable delivery would cost seq/ack/retransmit state for nothing.
//!   RAW frames are unordered and unnumbered, by design.
//!
//! ## MTU (feature 3)
//!
//! The datagram budget (`max_datagram_bytes`, default 1472 =
//! MTU 1500 − IP 20 − UDP 8) is enforced on the **outbound** path: an
//! encoded datagram over the budget is **dropped and counted** (warned
//! once per session), not fragmented and not refused. Rationale: a
//! refusal mid-stream would break a live session; fragmentation means
//! per-session reassembly state in the client (declared out of scope for
//! v1); and the dropped frame is a *snapshot* — the self-healing band —
//! whose staleness is bounded by the keep-alive. The room-side
//! `max_snapshot_bytes` warning (default 1400 + 7 header bytes < 1472)
//! is the standing signal to split the snapshot group instead.
//!
//! ## Session teardown (feature 4)
//!
//! UDP has no FIN. The previous turn's `idle_timeout` mechanism carries
//! over as the demux's **deadline heap**: every session's last-seen
//! instant + idle window is a lazy-invalidation entry in a
//! `BTreeSet<(Instant, SocketAddr)>`; the demux loop arms
//! `timeout(min_deadline, recv_from)` — the SAME single-future idiom the
//! TCP reader pump uses (the deadline fires only while the read stays
//! pending; a ready datagram always wins). When it fires, overdue
//! sessions get `ConnIn::ServerClosed` (the actor answers `ERROR` 9 and
//! tears down) and are removed. A second path removes a session whose
//! actor is already gone: the next datagram for it hits a closed
//! mailbox. No per-session timer tasks, no multiplexing, no shared
//! state — the heap is the demux task's local state, exactly like an
//! actor's counter.
//!
//! ## The scale question (single demux at 100k)
//!
//! One task demuxes every datagram. Per datagram: one `recv_from`
//! syscall (~0.1–0.3 µs), one hash lookup, one `BTreeSet` insert
//! `O(log N)` ≈ 17 steps, one `try_send` — ≈ 0.5 µs of bookkeeping. At
//! 100k sessions × 2 datagrams/s ≈ 0.1 core; even at 10/s per session
//! (3M/s total, the `all`-visibility fan-out ceiling) the bookkeeping is
//! ≈ 1.5 cores — the same order as the per-session writer tasks' send
//! syscalls (which are identical in number to TCP's). The real 100k
//! wall is the kernel's single-socket pps ceiling and the fan-out
//! volume (the visibility strategy), the same class of wall TCP has —
//! see `docs/ROADMAP.md` (rUDP turn) for the full arithmetic and the
//! alternatives compared (SO_REUSEPORT sharding rejected: it saves
//! <5% of the cost and breaks the single-`accept()` future / the shared
//! connection-id space; per-due O(N) sweeps rejected: 80× the cost of
//! the heap under join churn).

mod client;
mod cookie;
mod demux;
mod transport;
mod wire;
mod writer;

#[cfg(test)]
mod tests;

pub use client::{UdpClient, UdpClientStats};
pub use transport::{UdpTransport, UdpTransportConfig};

// Re-homed internals: each lives in the module that owns its concern,
// and is named here so every child module reaches it by one path.
use cookie::CookieKey;
use demux::{UdpSession, demux};
use wire::{body_of, encode_ack, encode_hello, encode_raw, encode_rel};
use writer::udp_pump_spawner;

use std::time::Duration;

use gsb_protocol::op;


/// Datagram kinds (see module docs).
pub const KIND_RAW: u8 = 0;
pub const KIND_REL: u8 = 1;
pub const KIND_ACK: u8 = 2;
pub const KIND_HELLO: u8 = 3;

/// The retransmit interval of the reliable control band.
const RETRANSIT_RTO: Duration = Duration::from_millis(50);
/// Max age of an un-ACKed reliable frame before it is given up (counted).
const RETRANSIT_MAX: Duration = Duration::from_millis(250);
/// Out-of-order window of the reliable receiver (control frames per
/// direction are rare; 16 is far beyond any realistic gap).
const OOB_CAP: usize = 16;
/// The rUDP endpoint channel capacity (established sessions awaiting the
/// accept loop; 1024 ≈ 1024 handshakes in a few ms — pathological).
const ENDPOINT_CHANNEL: usize = 1024;

/// The datagram budget: MTU 1500 − IP 20 − UDP 8 (the IPv4 path).
pub const DEFAULT_MAX_DATAGRAM_BYTES: usize = 1472;

/// A frame is control-band (reliable) when its opcode is in the base
/// band, except the transport markers themselves (which never travel as
/// frames — the demux consumes them; `UDP_ACK` is synthesized by the
/// demux and consumed by the writer's retransmit state).
fn is_control(op: u16) -> bool {
    (1..=64).contains(&op) && op != op::base::UDP_ACK
}

/// splitmix64 — the per-stage mix (no crypto needed; the process key is
/// the secrecy).
#[inline]
fn sm64(x: &mut u64) {
    *x = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *x;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    *x = z ^ (z >> 31);
}
