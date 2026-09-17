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
//! server → HELLO { nonce, cookie: F(nonce, peer, key, slot) }
//! client → HELLO { nonce, cookie }           (proof)
//! server: cookie verified → session established (channels pre-created)
//! ```
//!
//! `F` is a per-process-keyed mix (splitmix64 folds of `nonce`, the peer
//! address, the key and the time slot) — not a KDF: v1 has no crypto
//! layer (out of scope), and the property needed is "unforgeable over the
//! network without the key". The challenge response is well-formed only
//! for a well-formed challenge (same 18-byte size), so forged-traffic
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
//! ## Cookie rotation (why a captured proof expires)
//!
//! The key alone is not enough. With `F` a function of (key, nonce,
//! peer) only, a proof observed once on the wire stays valid **for the
//! life of the process**: anyone who can replay it from the same apparent
//! address re-establishes a session whenever they like, and the
//! handshake's whole job — "prove you own this return path, now" — loses
//! the "now".
//!
//! So `F` takes a fourth term: a **time slot** ([`CookieClock`]), an
//! integer counter of [`COOKIE_SLOT`] periods since bind. The server
//! mints the challenge for the current slot and accepts a proof for the
//! current slot **or the previous one**. Nothing else changes:
//!
//! - **the wire is untouched** — still `3 HELLO [u64 nonce][u64 cookie]`,
//!   18 bytes each way. The slot is not sent: both sides of the server's
//!   own computation read it from the same clock, and the client never
//!   needs to know it exists;
//! - **the handshake stays stateless** — the slot is recomputed from an
//!   `Instant` at verification time. No pre-handshake table, no timer
//!   task, no shared rotation state, nothing to lock. The demux keeps its
//!   single awaited source;
//! - **the key stays the secret** — entropy-derived, never clock-derived
//!   (see [`CookieKey`]). The slot is a public counter and is folded WITH
//!   the key precisely because it is public. Keep the two apart: the KEY
//!   is unpredictability, the SLOT is expiry. The test names say which is
//!   which.
//!
//! **The interval: 10 s, so a replay window of 10-20 s.** A proof is
//! usable until the end of the slot after the one it was issued in, so
//! the window is one to two periods depending on where in its slot the
//! proof was minted. Sized against the two real numbers:
//!
//! - *below*, the handshake it must not break. A proof is produced one
//!   RTT after the challenge is received, plus whatever the client's
//!   scheduler adds — call it 300 ms on a bad link, a couple of seconds
//!   for a phone whose radio was asleep. 10 s of grace is 3-30× that, so
//!   a legitimate handshake never loses a race with the rotation (and if
//!   one somehow did, the client's own retry mints a fresh challenge —
//!   the server is idempotent in it);
//! - *above*, the exposure it leaves. 10-20 s is short enough that a
//!   captured proof is worthless by the time any realistic capture →
//!   replay pipeline turns it around, and long enough that the rotation
//!   costs exactly nothing at runtime (two integer divisions per
//!   handshake, no state).
//!
//! **Rejected — a timestamp inside the cookie's bits.** Spending, say,
//! 16 of the cookie's 64 bits on a coarse issue time would let the server
//! verify a single slot with an explicit age check. It buys nothing here
//! (the two-slot acceptance already expresses the same policy) and costs
//! the thing the cookie actually rests on: an unforgeable value narrowed
//! from 64 bits to 48.
//!
//! **Rejected — a remembered set of issued cookies.** Exact
//! single-use semantics, and exactly the per-unverified-peer allocation
//! the stateless handshake exists to avoid. An attacker's forged
//! challenge requests would size the table.
//!
//! **Rejected — rotating the KEY on an interval instead.** Same
//! observable behaviour, but it puts a mutable secret where a constant
//! used to be (two keys live at once, both written from the demux task,
//! and `bind`'s "entropy or refuse to start" guarantee now has to hold
//! for every re-draw at runtime). The slot term achieves the expiry with
//! the key still immutable — drawn once, at bind, exactly as tested.
//!
//! //! `UDP_HELLO`/`UDP_ACK` opcodes live in the base band but are
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
//! suite; NOT hardened for lossy real-world networks. The silent REL
//! give-up this paragraph used to name is closed — see "The REL liveness
//! bound" below. Production deployments should still run a hardened
//! transport behind the same [`crate::Transport`] seam.
//!
//! ## Band semantics (feature 2)
//!
//! - **Control band (AUTH/JOIN/LEAVE/HEARTBEAT/ERROR): reliable for as
//!   long as the band is alive.** Loss would break correctness (a lost
//!   JOIN result hangs the client; a lost HEARTBEAT is tolerable only
//!   because HEARTBEAT_ACK is not state — but the *request* may be).
//!   Each direction keeps its own sequence: the sender retransmits the
//!   oldest un-ACKed REL frame every [`RETRANSIT_RTO`] until it is
//!   ACKed. An individual frame is **never** abandoned; the *band* is
//!   declared dead as a whole (see "The REL liveness bound" below). The
//!   receiver deduplicates (cumulative), buffers a small out-of-order
//!   window, and only advances when the gap fills — control frames are
//!   never delivered out of order (AUTH before JOIN is a property of
//!   `seq`, not of luck).
//! - **Snapshot band (WORLD_SNAPSHOT & co.): RAW.** The room's snapshots
//!   are self-contained (a client that loses one heals on the next
//!   snapshot or the keep-alive resend — `RoomConfig::keepalive_hz`), so
//!   reliable delivery would cost seq/ack/retransmit state for nothing.
//!   RAW frames are unordered and unnumbered, by design.
//!
//! ## The REL liveness bound (why a give-up is not a statistic)
//!
//! v1 popped a REL frame that had gone un-ACKed for 250 ms, incremented
//! a counter and continued. That was the transport's worst failure mode,
//! because it was **silent**: the cumulative receiver never advances past
//! the hole, so the direction is wedged forever, while the session stays
//! alive and the RAW game band keeps flowing. A client whose
//! `JOIN_ROOM_RESULT` was the abandoned frame waits forever — no error,
//! no retry, no close — and the server holds its room slot and registry
//! row the whole time.
//!
//! **Decision: memory-bounded retransmit + a no-ACK-progress death
//! threshold.** A frame is retransmitted for as long as the band is
//! alive. The band is declared dead when the sender's cumulative ACK has
//! not advanced *at all* for [`REL_NO_ACK_FATAL`] while something is
//! outstanding, or when the un-ACKed queue reaches [`RETRANSIT_CAP`].
//! Death is **session-fatal and loud**: the writer hands
//! `ConnIn::ServerClosed` to the connection actor over its mailbox — an
//! in-process channel, never the socket, which is precisely what is in
//! doubt — and the actor runs its ordinary teardown (final metrics
//! flush, `RegistryMsg::ConnClosed`). From there the death is
//! indistinguishable from a dropped TCP socket: the registry releases the
//! row of an unaffiliated session outright and routes a DETACH for a room
//! member, whose `on_disconnect` policy owns the entity and its slot from
//! then on (`docs/RECONNECT.md` §4).
//!
//! **Why "no ACK progress for X" and not "this frame aged out":** they
//! differ exactly where it matters. Per-frame aging measures one
//! datagram's luck; 250 ms of loss is an ordinary event on a bad mobile
//! link (an LTE handover is tens of ms, a Wi-Fi roam 100-500 ms, a
//! Wi-Fi↔cellular switch 1-3 s), so a per-frame bound would kill
//! sessions that today merely stutter — trading a silent bug for a noisy
//! one. Cumulative-ACK progress measures the *channel*: as long as the
//! peer confirms anything, the link is working and every outstanding
//! frame is still going to arrive.
//!
//! **The threshold: 5 s.** It clears every stutter above with headroom
//! (5-15× a Wi-Fi roam, ~2× the worst Wi-Fi↔cellular switch), and it is
//! ~100 retransmissions of the same frame at the 50 ms RTO: a path that
//! delivers nothing in 100 tries over 5 s is not stuttering, it is down.
//! It also sits well below the demux's 30 s `idle_timeout`, which is the
//! only other death signal — and that one watches INBOUND silence, so a
//! peer that keeps sending RAW input while never ACKing (the exact
//! reported shape) is invisible to it.
//!
//! **Rejected — give-up ⇒ session-fatal (the other named candidate).**
//! Correct about the consequence (an undeliverable control frame must
//! end the session) and wrong about the trigger: at `RETRANSIT_MAX`
//! = 250 ms it declares death after five attempts, so a single handover
//! blackout disconnects a healthy player. Raising `RETRANSIT_MAX` to a
//! survivable value turns it into this design with a worse name — the
//! per-frame clock only *approximates* channel liveness, and does so
//! badly under bursty loss (the first frame after a 4 s quiet period
//! carries the whole blackout in its own age).
//!
//! **Rejected — RTO backoff + a retry count (the TCP shape).** A retry
//! budget with exponential backoff expresses the same bound in units
//! nobody can reason about (how many retries is 5 s? depends on the
//! backoff curve), and backoff needs RTT estimation to be worth
//! anything, which this transport does not have yet (see "What is still
//! open"). The wall-clock bound is the honest statement of the policy;
//! adding backoff later changes the retransmit *schedule* without
//! touching the death rule.
//!
//! **Rejected — tell the client instead of closing.** There is no path
//! to tell it: the ERROR frame is itself a control-band frame and would
//! queue behind the hole. Anything we could still deliver (a RAW
//! notice) would need a new opcode outside the message table and a
//! client that understands it — a protocol change to make a dead
//! session marginally more polite.
//!
//! **The memory bound.** With no per-frame give-up the un-ACKed queue is
//! no longer bounded by the 250 ms clock, so it is bounded explicitly:
//! [`RETRANSIT_CAP`] frames per direction per session. Control frames
//! are small (tens of bytes), so the realistic ceiling is a few KB;
//! the absolute one (every frame at the full datagram budget) is
//! ~380 KB, and it is only reachable by a session whose ACKs have
//! already stopped — i.e. one already inside its dying window.
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
use cookie::{CookieClock, CookieKey};
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

/// The rotation period of the handshake cookie's TIME TERM (see
/// [`CookieClock`], and the module docs, "Cookie rotation"). A proof is
/// accepted for its own slot and the previous one, so the replay window
/// a captured proof leaves open is between one and two of these.
const COOKIE_SLOT: Duration = Duration::from_secs(10);

/// The retransmit interval of the reliable control band.
const RETRANSIT_RTO: Duration = Duration::from_millis(50);
/// How long the reliable band may make NO cumulative-ACK progress at all
/// — while something is outstanding — before that direction is declared
/// dead and the session ends. See the module docs, "The REL liveness
/// bound", for why the bound is on the CHANNEL and not on a frame's age,
/// and for the 5 s figure against real lossy-link numbers.
const REL_NO_ACK_FATAL: Duration = Duration::from_secs(5);
/// Memory bound of the un-ACKed retransmit queue (per direction, per
/// session). Reaching it means the peer has confirmed nothing while this
/// many control frames piled up, which is the same death as
/// [`REL_NO_ACK_FATAL`] arriving early. Sized far above any legitimate
/// burst: the control band is AUTH/JOIN/LEAVE/HEARTBEAT_ACK/ERROR, and
/// the actor's own guardrails (violation answer limit, pre-auth frame
/// budget) already cap how many of those a client can provoke.
const RETRANSIT_CAP: usize = 256;
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
