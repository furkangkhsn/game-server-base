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
//! ## Band semantics (feature 2)
//!
//! - **Control band (AUTH/JOIN/LEAVE/HEARTBEAT/ERROR): reliable.** Loss
//!   would break correctness (a lost JOIN result hangs the client; a
//!   lost HEARTBEAT is tolerable only because HEARTBEAT_ACK is not
//!   state — but the *request* may be). Each direction keeps its own
//!   sequence: the sender retransmits the oldest un-ACKed REL frame
//!   every `RETRANSIT_RTO` until it is ACKed or `RETRANSIT_MAX` elapses
//!   (give-up, counted). The receiver deduplicates (cumulative), buffers
//!   a small out-of-order window, and only advances when the gap
//!   fills — control frames are never delivered out of order (AUTH
//!   before JOIN is a property of `seq`, not of luck).
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

use std::collections::{BTreeSet, HashMap, VecDeque};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use crossbeam_channel::{Receiver, Sender};
use gsb_core::channel::{FrameBatch, Inbox, Mailbox, channel};
use gsb_core::conn::ConnIn;
use gsb_core::id::ConnectionId;
use gsb_protocol::op;
use gsb_protocol::FrameBody;
use tokio::net::UdpSocket;
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

use crate::transport::{BoxFuture, Endpoint, Listener, PumpSpawner, Transport};

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

/// The per-process cookie key: 16 bytes (two u64 words), either the
/// operator's config (`cookie_key`) or a draw from the **OS entropy
/// source** at bind time (see the module docs, "The key").
///
/// **Why OS entropy and not the wall clock:** an off-path attacker who
/// can narrow the server's *start time* to a small window enumerates a
/// clock-derived key offline and forges proofs without ever receiving
/// the challenge. OS entropy is uniform and independent of anything an
/// attacker can observe; 64 bits of it would already be enough, and 128
/// bits is the same draw.
///
/// **No silent degradation:** if the entropy source cannot be read and
/// the operator did not supply a key, the bind fails (the server
/// refuses to start). The key is the entire basis of the
/// anti-amplification property: a predictable key does not weaken it, it
/// *inverts* it (an attacker who knows the key forges proofs for
/// spoofed addresses and allocates full sessions — channels, actor,
/// registry entry — per fake peer), and "the server started with a
/// warning" is not a posture an operator can be trusted to notice.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CookieKey(u64, u64);

impl CookieKey {
    /// Build the key from operator-supplied bytes (the composition root
    /// parses the config's 32-hex-char string into these). Pure and
    /// deterministic: the config path is a function of the operator's
    /// input alone.
    fn from_bytes(b: [u8; 16]) -> Self {
        Self(
            u64::from_le_bytes(b[0..8].try_into().unwrap()),
            u64::from_le_bytes(b[8..16].try_into().unwrap()),
        )
    }

    /// Draw the key from the OS entropy source. The `Err` arm is a
    /// deliberate, loud choice (see the struct docs): [`Self::generate`]
    /// is called from `bind`, which maps the failure to an `io::Error`
    /// and the server refuses to start.
    fn generate() -> Result<Self, std::io::Error> {
        let mut b = [0u8; 16];
        getrandom::fill(&mut b).map_err(|e| {
            std::io::Error::other(format!(
                "cannot read OS entropy for the rUDP cookie key: {e}"
            ))
        })?;
        Ok(Self::from_bytes(b))
    }

    /// F(nonce, peer, key) — the stateless cookie.
    fn compute(&self, nonce: u64, peer: SocketAddr) -> u64 {
        let (ip, port) = match peer {
            SocketAddr::V4(v4) => (v4.ip().to_bits() as u64, v4.port()),
            SocketAddr::V6(v6) => {
                let w = v6.ip().octets();
                let hi = u64::from_be_bytes(w[0..8].try_into().unwrap());
                let mut lo = u64::from_be_bytes(w[8..16].try_into().unwrap());
                sm64(&mut lo);
                (hi ^ lo, v6.port())
            }
        };
        let mut x = self.0 ^ nonce;
        sm64(&mut x);
        let mut t = ip ^ self.1;
        sm64(&mut t);
        x ^= t;
        x ^= (port as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15);
        sm64(&mut x);
        x
    }
}

// ── datagram encoding/parsing (shared by server and client) ─────────

/// Encode a RAW (lossy game-band) datagram.
fn encode_raw(frame: &FrameBody) -> Vec<u8> {
    let body = frame.encode();
    let mut d = Vec::with_capacity(1 + body.len());
    d.push(KIND_RAW);
    d.extend_from_slice(&body);
    d
}

/// Encode a REL (reliable control-band) datagram.
fn encode_rel(seq: u32, frame: &FrameBody) -> Vec<u8> {
    let body = frame.encode();
    let mut d = Vec::with_capacity(5 + body.len());
    d.push(KIND_REL);
    d.extend_from_slice(&seq.to_le_bytes());
    d.extend_from_slice(&body);
    d
}

/// Encode an ACK datagram (cumulative: the next expected seq).
fn encode_ack(next: u32) -> Vec<u8> {
    let mut d = [0u8; 5];
    d[0] = KIND_ACK;
    d[1..5].copy_from_slice(&next.to_le_bytes());
    d.to_vec()
}

/// Encode a HELLO datagram.
fn encode_hello(nonce: u64, cookie: u64) -> Vec<u8> {
    let mut d = [0u8; 18];
    d[0] = KIND_HELLO;
    d[1..9].copy_from_slice(&nonce.to_le_bytes());
    d[9..17].copy_from_slice(&cookie.to_le_bytes());
    d.to_vec()
}

/// Parse the frame body out of a RAW/REL datagram (2-byte op + payload).
fn body_of(d: &[u8], header: usize) -> Option<FrameBody> {
    let body = &d[header..];
    if body.len() < 2 {
        return None;
    }
    let op = u16::from_le_bytes([body[0], body[1]]);
    Some(FrameBody::new(op, Bytes::copy_from_slice(&body[2..])))
}

// ── the server side ──────────────────────────────────────────────────

/// rUDP transport configuration (set by the composition root from the
/// server config — the channel capacities mirror `conn_inbox`/`conn_out`
/// because the demux pre-creates them at handshake).
#[derive(Debug, Clone)]
pub struct UdpTransportConfig {
    pub inbox_capacity: usize,
    pub outbox_capacity: usize,
    /// The datagram budget (feature 3; default [`DEFAULT_MAX_DATAGRAM_BYTES`]).
    pub max_datagram_bytes: usize,
    /// The session idle window (feature 4; `None` disables the sweep).
    pub idle_timeout: Option<Duration>,
    /// Operator-supplied cookie key (16 bytes; the composition root
    /// parses the config's 32-hex-char string into these). `None` = draw
    /// from the OS entropy source at bind time. See [`CookieKey`].
    pub cookie_key: Option<[u8; 16]>,
}

impl Default for UdpTransportConfig {
    fn default() -> Self {
        Self {
            inbox_capacity: 1024,
            outbox_capacity: 256,
            max_datagram_bytes: DEFAULT_MAX_DATAGRAM_BYTES,
            idle_timeout: Some(Duration::from_secs(30)),
            cookie_key: None,
        }
    }
}

/// The rUDP transport: one socket, one shared demux, many sessions.
#[derive(Debug, Clone, Default)]
pub struct UdpTransport {
    pub config: UdpTransportConfig,
}

struct UdpListenerHandle {
    /// The shared socket (for `local_addr`; the demux and the per-session
    /// writers hold their own clones — the socket's fd outlives the
    /// listener and is released as the connections drain).
    sock: Arc<UdpSocket>,
    /// The endpoint stream from the demux. A crossbeam receiver on
    /// purpose: `recv`/`try_recv` take `&self`, so the receiver can live
    /// inside the `Arc<Self>` behind the `Listener` trait (a tokio mpsc
    /// receiver needs `&mut` — unreachable through an `Arc` without a
    /// lock, and locks are banned in this workspace).
    end_rx: Receiver<Endpoint>,
    demux: JoinHandle<()>,
}

impl Transport for UdpTransport {
    fn bind(
        self: Arc<Self>,
        addr: SocketAddr,
    ) -> BoxFuture<'static, std::io::Result<Arc<dyn Listener>>> {
        Box::pin(async move {
            let sock = Arc::new(UdpSocket::bind(addr).await?);
            // (The kernel receive queue is the first line of buffering
            // for every session — there is no per-connection socket.
            // Tuning it (SO_RCVBUF) would need the raw fd; left at the
            // system default in v1 — see ROADMAP "v1 constraints".)
            let (end_tx, end_rx) = crossbeam_channel::bounded(ENDPOINT_CHANNEL);
            // The cookie key: the operator's config, or the OS entropy
            // source. A failure here is deliberate (see `CookieKey`): a
            // predictable key inverts the anti-amplification property,
            // so the bind errors out and the server refuses to start
            // rather than run weak.
            let key_source = if self.config.cookie_key.is_some() {
                "config"
            } else {
                "os-entropy"
            };
            let key = match self.config.cookie_key {
                Some(bytes) => CookieKey::from_bytes(bytes),
                None => CookieKey::generate().map_err(|e| {
                    std::io::Error::other(format!(
                        "{e} (supply one explicitly via the config's \
                         udp_cookie_key: 32 hex characters)"
                    ))
                })?,
            };
            let cfg = self.config.clone();
            let demux = tokio::spawn(demux(
                sock.clone(),
                end_tx,
                key,
                cfg.inbox_capacity,
                cfg.outbox_capacity,
                cfg.max_datagram_bytes,
                cfg.idle_timeout,
            ));
            info!(%addr, %key_source, "rUDP transport bound (shared demux started)");
            Ok(Arc::new(UdpListenerHandle {
                sock,
                end_rx,
                demux,
            }) as Arc<dyn Listener>)
        })
    }
}

impl Listener for UdpListenerHandle {
    fn accept(self: Arc<Self>) -> BoxFuture<'static, std::io::Result<Endpoint>> {
        Box::pin(async move {
            // A crossbeam `recv` parks its thread, so it runs on the
            // blocking pool (one parked blocking thread per PENDING
            // accept; endpoints are rare — handshakes — so this never
            // scales with traffic). The demux side is non-blocking
            // (`try_send`), so a slow accept loop can never stall the
            // demux (and therefore every other session).
            let rx = self.end_rx.clone();
            match tokio::task::spawn_blocking(move || rx.recv())
                .await
            {
                Ok(Ok(endpoint)) => Ok(endpoint),
                Ok(Err(_)) | Err(_) => Err(std::io::Error::other("rUDP demux gone")),
            }
        })
    }

    fn local_addr(&self) -> Option<SocketAddr> {
        self.sock.local_addr().ok()
    }

    fn close(&self) {
        // Stop the shared demux: aborting it drops its socket clone and
        // its endpoint sender (the accept loop's `recv` then fails and
        // ends). The per-session writers are not touched here: they exit
        // with their connections (the actor cascade) and release their
        // socket clones on the way.
        self.demux.abort();
    }
}

/// One established session's transport state (all of it local to the
/// demux task — the demux is the actor; its map is its counter).
#[derive(Debug)]
struct UdpSession {
    in_tx: Mailbox<ConnIn>,
    /// Kept for ACK piggyback (the demux hands inbound ACKs to the
    /// session's writer through the outbound channel).
    out_tx: Mailbox<FrameBatch>,
    last_seen: Instant,
    /// Inbound reliable state (client→server): the next expected seq,
    /// plus the small out-of-order window awaiting the gap.
    in_expected: u32,
    in_oob: HashMap<u32, Vec<u8>>,
    oob_dropped: u64,
    dup_in: u64,
    /// Inbound frames dropped on a full (bounded) session mailbox.
    inbox_full: u64,
    inbox_full_warned: bool,
}

#[derive(Debug)]
struct Demux {
    sock: Arc<UdpSocket>,
    end_tx: Sender<Endpoint>,
    cookie: CookieKey,
    inbox_cap: usize,
    outbox_cap: usize,
    max_datagram: usize,
    idle: Option<Duration>,
    sessions: HashMap<SocketAddr, UdpSession>,
    /// Idle deadlines (feature 4): (deadline, peer) with lazy
    /// invalidation — an entry is *current* only while it equals
    /// `session.last_seen + idle`; datagrams supersede it (a new entry
    /// is pushed); the sweep discards stale tops.
    deadlines: BTreeSet<(Instant, SocketAddr)>,
    buf: Vec<u8>,
    // lifetime counters (reported once at demux exit):
    established: u64,
    challenges: u64,
    bad_cookie: u64,
    endpoints_dropped: u64,
    swept_idle: u64,
    removed_actor_gone: u64,
    acks_piggybacked: u64,
    ack_piggyback_failed: u64,
    oversized_in: u64,
    bad_datagrams: u64,
}

impl Demux {
    fn remove_session(&mut self, peer: SocketAddr) {
        // Drop the CURRENT deadline entry (exact match); stale ones are
        // lazily discarded by the sweep.
        if let Some(idle) = self.idle
            && let Some(s) = self.sessions.get(&peer)
        {
            self.deadlines.remove(&(s.last_seen + idle, peer));
        }
        self.sessions.remove(&peer);
    }

    /// Forward one decoded frame into the session's mailbox; return
    /// `true` if the session must be removed (its actor is gone). The
    /// caller must then call `remove_session`.
    fn forward(&mut self, peer: SocketAddr, fb: FrameBody) -> bool {
        let Some(s) = self.sessions.get_mut(&peer) else {
            return false;
        };
        // The peer is alive: reset its idle window (push the new entry;
        // the old one becomes stale and is swept lazily). Copy out of the
        // borrow first — the deadline insert touches a different field.
        let now = Instant::now();
        let idle = self.idle;
        s.last_seen = now;
        if let Some(idle) = idle {
            self.deadlines.insert((now + idle, peer));
        }
        match s.in_tx.try_send(ConnIn::Frame(fb)) {
            Ok(()) => false,
            Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                // Backpressure without a per-connection socket: a full
                // mailbox means this session's actor is stuck
                // (downstream — room/registry — is not consuming). Drop
                // the frame, stay isolated (never stall the demux, i.e.
                // every other session), count it.
                s.inbox_full += 1;
                if !s.inbox_full_warned {
                    s.inbox_full_warned = true;
                    warn!(
                        %peer,
                        "rUDP: session mailbox full; inbound frames for this \
                         session are being dropped (isolated: other sessions \
                         are unaffected)"
                    );
                }
                false
            }
            Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
                // The actor exited (budget close, shutdown, …): the
                // caller removes the session (the idle sweep would find
                // it too, but this is immediate).
                self.removed_actor_gone += 1;
                true
            }
        }
    }

    /// Inbound reliable (client→server): dedupe + order + forward + ACK.
    ///
    /// Structured as two phases: PHASE 1 decides, under the session
    /// borrow, what to forward (in sequence order) and whether to ACK;
    /// PHASE 2 performs the forwarding/ACK via `self` methods (which
    /// must not run while the session borrow is live).
    fn handle_rel(&mut self, peer: SocketAddr, seq: u32, body: Vec<u8>) {
        // PHASE 1.
        let (to_forward, ack_to) = {
            let Some(s) = self.sessions.get_mut(&peer) else {
                return; // no session: drop (pre-handshake or already gone)
            };
            // The peer is alive: reset its idle window.
            let now = Instant::now();
            let idle = self.idle;
            s.last_seen = now;
            if let Some(idle) = idle {
                self.deadlines.insert((now + idle, peer));
            }
            let mut to_forward: Vec<FrameBody> = Vec::new();
            let mut ack_to: Option<u32> = None;
            match seq.cmp(&s.in_expected) {
                std::cmp::Ordering::Equal => {
                    if let Some(fb) = body_of(&body, 0) {
                        to_forward.push(fb);
                    }
                    s.in_expected = s.in_expected.wrapping_add(1);
                    // Flush the contiguous tail of the out-of-order
                    // window (control frames are delivered in sequence
                    // order, never out of it).
                    while let Some(gap) = s.in_oob.remove(&s.in_expected) {
                        if let Some(fb) = body_of(&gap, 0) {
                            to_forward.push(fb);
                        }
                        s.in_expected = s.in_expected.wrapping_add(1);
                    }
                    ack_to = Some(s.in_expected);
                }
                std::cmp::Ordering::Less => {
                    // Duplicate (its ACK was presumably lost): re-ACK
                    // only — never re-forward (correctness: control
                    // frames run exactly once, in order).
                    s.dup_in += 1;
                    ack_to = Some(s.in_expected);
                }
                std::cmp::Ordering::Greater => {
                    // Gap: buffer (bounded) and do NOT advance the
                    // cumulative ACK — the client retransmits the
                    // missing frame.
                    if s.in_oob.len() < OOB_CAP {
                        s.in_oob.insert(seq, body);
                    } else {
                        s.oob_dropped += 1;
                    }
                }
            }
            (to_forward, ack_to)
        };
        // PHASE 2 (the borrow above is over).
        for fb in to_forward {
            if self.forward(peer, fb) {
                self.remove_session(peer);
                return;
            }
        }
        if let Some(next) = ack_to {
            self.send_ack(peer, next);
        }
    }

    fn send_ack(&mut self, peer: SocketAddr, next: u32) {
        let ack = encode_ack(next);
        if let Err(e) = self.sock.try_send_to(&ack, peer) {
            debug!(%peer, %e, "rUDP: ack send failed (best-effort)");
        }
    }

    fn handle_hello(&mut self, peer: SocketAddr) {
        let nonce = u64::from_le_bytes(self.buf[1..9].try_into().unwrap());
        let cookie = u64::from_le_bytes(self.buf[9..17].try_into().unwrap());
        // An established peer must not re-handshake: ignore (its session
        // is keyed by this address; NAT rebind means a NEW address).
        if self.sessions.contains_key(&peer) {
            return;
        }
        let expected = self.cookie.compute(nonce, peer);
        if cookie == 0 {
            // Challenge request: answer with the proof (stateless — no
            // state allocated before the proof; the response is the same
            // size as the request, so forged traffic cannot amplify).
            self.challenges += 1;
            let hello = encode_hello(nonce, expected);
            if let Err(e) = self.sock.try_send_to(&hello, peer) {
                debug!(%peer, %e, "rUDP: challenge send failed");
            }
        } else if cookie == expected {
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
            let endpoint = Endpoint::new(udp_pump_spawner(
                self.sock.clone(),
                peer,
                self.max_datagram,
            ))
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
            // Forged or stale proof: drop, count, answer nothing.
            self.bad_cookie += 1;
        }
    }

    fn handle(&mut self, n: usize, peer: SocketAddr) {
        if n < 1 || n > self.max_datagram {
            self.oversized_in += 1;
            return;
        }
        match self.buf[0] {
            KIND_HELLO => {
                if n < 18 {
                    self.bad_datagrams += 1;
                    return;
                }
                self.handle_hello(peer);
            }
            KIND_ACK => {
                if n < 5 {
                    self.bad_datagrams += 1;
                    return;
                }
                let ack = u32::from_le_bytes(self.buf[1..5].try_into().unwrap());
                // Piggyback the ACK into the session's OUTBOUND channel:
                // the writer (the only reader of it) applies it to its
                // retransmit state. This is the one transport-internal
                // round trip: it needs no command channel (the demux
                // cannot await one — its only awaited source is the
                // socket) and costs one bounded-channel send.
                if let Some(s) = self.sessions.get(&peer) {
                    let fb =
                        FrameBody::new(op::base::UDP_ACK, Bytes::from(ack.to_le_bytes().to_vec()));
                    match s.out_tx.try_send(vec![fb]) {
                        Ok(()) => self.acks_piggybacked += 1,
                        Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                            // The writer is stuck; the retransmit will
                            // run to RETRANSIT_MAX and give up (bounded).
                            self.ack_piggyback_failed += 1;
                        }
                        Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
                            self.removed_actor_gone += 1;
                            self.remove_session(peer);
                        }
                    }
                }
            }
            KIND_REL => {
                if n < 7 {
                    self.bad_datagrams += 1;
                    return;
                }
                let seq = u32::from_le_bytes(self.buf[1..5].try_into().unwrap());
                let body = self.buf[5..n].to_vec();
                self.handle_rel(peer, seq, body);
            }
            KIND_RAW => {
                if let Some(fb) = body_of(&self.buf[1..n], 0) {
                    // RAW (lossy game band): no seq, no order, no ACK.
                    if self.forward(peer, fb) {
                        self.remove_session(peer);
                    }
                } else {
                    self.bad_datagrams += 1;
                }
            }
            _ => self.bad_datagrams += 1,
        }
    }

    /// The idle sweep (feature 4): pop overdue entries; a LIVE one
    /// (still equal to `last_seen + idle`) means the session has been
    /// silent for the whole window: notify the actor (best-effort) and
    /// remove it. Stale entries (superseded by a datagram) are discarded.
    fn sweep(&mut self) {
        let Some(idle) = self.idle else {
            return;
        };
        let now = Instant::now();
        while let Some(&(deadline, peer)) = self.deadlines.first() {
            if deadline > now {
                break;
            }
            self.deadlines.remove(&(deadline, peer));
            let live = self
                .sessions
                .get(&peer)
                .map(|s| s.last_seen + idle == deadline)
                .unwrap_or(false);
            if !live {
                continue; // stale entry
            }
            let Some(session) = self.sessions.remove(&peer) else {
                continue;
            };
            self.swept_idle += 1;
            let reason = format!("idle timeout: no client traffic for {idle:?}");
            match session.in_tx.try_send(ConnIn::ServerClosed {
                reason: reason.clone(),
            }) {
                Ok(()) => {
                    // The actor will answer ERROR 9 and tear down (its
                    // writer keeps sending until it exits; the session
                    // is already un-routable, so stray inbound for this
                    // peer is dropped).
                    warn!(%peer, %reason, "rUDP: session idle-swept");
                }
                Err(_) => {
                    // The actor is already gone: nothing to tell.
                    self.removed_actor_gone += 1;
                    debug!(%peer, "rUDP: idle sweep found an already-gone session");
                }
            }
        }
    }
}

/// The shared read path: one task, one awaited source (the socket read,
/// optionally wrapped in the min-idle-deadline — the same single-future
/// idiom as the TCP reader pump's idle timeout), one local state map.
async fn demux(
    sock: Arc<UdpSocket>,
    end_tx: Sender<Endpoint>,
    cookie: CookieKey,
    inbox_cap: usize,
    outbox_cap: usize,
    max_datagram: usize,
    idle: Option<Duration>,
) {
    let mut d = Demux {
        sock,
        end_tx,
        cookie,
        inbox_cap,
        outbox_cap,
        max_datagram,
        idle,
        sessions: HashMap::new(),
        deadlines: BTreeSet::new(),
        buf: vec![0u8; max_datagram + 64],
        established: 0,
        challenges: 0,
        bad_cookie: 0,
        endpoints_dropped: 0,
        swept_idle: 0,
        removed_actor_gone: 0,
        acks_piggybacked: 0,
        ack_piggyback_failed: 0,
        oversized_in: 0,
        bad_datagrams: 0,
    };
    loop {
        // Arm the read: if any session has an idle deadline pending, the
        // read is bounded by the EARLIEST one (the deadline fires only
        // while the read stays pending; a ready datagram always wins —
        // this wraps one future, it does not multiplex two sources).
        let wait = d
            .idle
            .and_then(|_| d.deadlines.first().map(|(deadline, _)| *deadline))
            .map(|deadline| deadline.saturating_duration_since(Instant::now()));
        let item = match wait {
            Some(w) => match tokio::time::timeout(w, d.sock.recv_from(&mut d.buf)).await {
                Ok(Ok(x)) => Some(Ok(x)),
                Ok(Err(e)) => Some(Err(e)),
                Err(_) => None, // the earliest deadline passed: sweep
            },
            None => match d.sock.recv_from(&mut d.buf).await {
                Ok(x) => Some(Ok(x)),
                Err(e) => Some(Err(e)),
            },
        };
        match item {
            Some(Ok((n, peer))) => d.handle(n, peer),
            Some(Err(e)) => {
                if e.kind() == std::io::ErrorKind::NotConnected {
                    break; // the listener closed the socket
                }
                warn!(%e, "rUDP demux: recv error; backing off");
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            None => d.sweep(),
        }
    }
    info!(
        established = d.established,
        challenges = d.challenges,
        bad_cookie = d.bad_cookie,
        endpoints_dropped = d.endpoints_dropped,
        swept_idle = d.swept_idle,
        removed_actor_gone = d.removed_actor_gone,
        acks_piggybacked = d.acks_piggybacked,
        ack_piggyback_failed = d.ack_piggyback_failed,
        oversized_in = d.oversized_in,
        bad_datagrams = d.bad_datagrams,
        "rUDP demux stopped"
    );
}

/// The per-session outbound pump spawner: ONLY a writer task (the reader
/// is the shared demux, owned by the listener).
fn udp_pump_spawner(
    sock: Arc<UdpSocket>,
    peer: SocketAddr,
    max_datagram: usize,
) -> PumpSpawner {
    Box::new(move |conn: ConnectionId,
          _in_tx: Mailbox<ConnIn>,
          out_rx: Inbox<FrameBatch>,
          _idle: Option<Duration>| {
        // `in_tx` is already registered in the demux (at handshake) and
        // `idle` is its deadline heap's concern — neither belongs to the
        // per-connection part.
        let writer = tokio::spawn(UdpWriter {
            conn,
            sock,
            peer,
            out_rx,
            max_datagram,
            seq: 0,
            acked: 1,
            retransmit: VecDeque::new(),
            dropped_oversized: 0,
            retransmits: 0,
            gave_up: 0,
            oversized_warned: false,
        }
        .run());
        (None, writer)
    })
}

/// The per-session writer: outbound batches → datagrams, with the
/// reliable control band (retransmit + cumulative-ack state) and the
/// MTU drop+count rule (feature 3). Local state only.
struct UdpWriter {
    conn: ConnectionId,
    sock: Arc<UdpSocket>,
    peer: SocketAddr,
    out_rx: Inbox<FrameBatch>,
    max_datagram: usize,
    /// Next outbound control seq (the client's first is 1, so the server
    /// hands out from 1 as well).
    seq: u32,
    /// Highest cumulative ACK received (the next expected seq).
    acked: u32,
    /// Un-ACKed outbound control frames: (seq, encoded datagram, sent_at).
    retransmit: VecDeque<(u32, Bytes, Instant)>,
    dropped_oversized: u64,
    retransmits: u64,
    gave_up: u64,
    oversized_warned: bool,
}

impl UdpWriter {
    async fn run(mut self) {
        loop {
            // One awaited source: the outbound channel, optionally bounded
            // by the retransmit interval (the deadline fires only while
            // the recv stays pending — a ready batch always wins).
            let batch = match tokio::time::timeout(RETRANSIT_RTO, self.out_rx.recv()).await {
                Ok(Some(b)) => Some(b),
                Ok(None) => break, // the actor (and the room) are gone
                Err(_) => None, // RTO: retransmit pass only
            };
            if let Some(batch) = batch {
                for frame in batch {
                    if frame.op == op::base::UDP_ACK {
                        // The demux's piggybacked inbound ACK (see
                        // `Demux::handle`): advance the cumulative state
                        // and prune the retransmit buffer.
                        let ok = frame.payload.len() >= 4;
                        if ok {
                            let ack =
                                u32::from_le_bytes(frame.payload[..4].try_into().unwrap());
                            self.acked = self.acked.max(ack);
                            while let Some(&(s, _, _)) = self.retransmit.front() {
                                if s < self.acked {
                                    self.retransmit.pop_front();
                                } else {
                                    break;
                                }
                            }
                        }
                        continue;
                    }
                    let control = is_control(frame.op);
                    let datagram = if control {
                        self.seq = self.seq.wrapping_add(1);
                        Bytes::from(encode_rel(self.seq, &frame))
                    } else {
                        Bytes::from(encode_raw(&frame))
                    };
                    // Feature 3: the datagram budget. Drop + count (the
                    // snapshot band is self-healing; the room-side
                    // max_snapshot_bytes warning is the standing signal).
                    if datagram.len() > self.max_datagram {
                        self.dropped_oversized += 1;
                        if !self.oversized_warned {
                            self.oversized_warned = true;
                            warn!(
                                conn = %self.conn,
                                peer = %self.peer,
                                op = frame.op,
                                size = datagram.len(),
                                budget = self.max_datagram,
                                "rUDP: frame exceeds the datagram budget; oversized \
                                 frames are dropped and counted (split the snapshot \
                                 group or lower its emission rate — the room-side \
                                 max_snapshot_bytes warning is the signal)"
                            );
                        }
                        continue;
                    }
                    if control {
                        self.retransmit
                            .push_back((self.seq, datagram.clone(), Instant::now()));
                    }
                    if let Err(e) = self.sock.send_to(&datagram, self.peer).await {
                        // The socket is shut (listener close) or the peer
                        // is gone: keep draining the channel so the
                        // actor's exit cascade is not delayed; the next
                        // send keeps failing until the channel closes.
                        debug!(conn = %self.conn, peer = %self.peer, %e, "rUDP: send failed");
                    }
                }
            }
            self.retransmit_pass();
        }
        if self.dropped_oversized > 0 || self.retransmits > 0 || self.gave_up > 0 {
            info!(
                conn = %self.conn,
                peer = %self.peer,
                dropped_oversized = self.dropped_oversized,
                retransmits = self.retransmits,
                gave_up = self.gave_up,
                "rUDP writer session counters"
            );
        }
        debug!(conn = %self.conn, "rUDP writer stopped");
    }

    /// Retransmit the oldest un-ACKed control frame whose RTO has passed;
    /// give up (counted) on frames older than RETRANSIT_MAX.
    fn retransmit_pass(&mut self) {
        let now = Instant::now();
        while let Some((_, datagram, sent)) = self.retransmit.front_mut() {
            if *sent + RETRANSIT_MAX <= now {
                self.retransmit.pop_front();
                self.gave_up += 1;
                continue;
            }
            if *sent + RETRANSIT_RTO <= now {
                match self.sock.try_send_to(datagram, self.peer) {
                    Ok(_) => {
                        *sent = now;
                        self.retransmits += 1;
                    }
                    Err(_) => break, // socket busy/closed: retry next pass
                }
            } else {
                break;
            }
        }
    }
}

// ── the client side (shared by the e2e tests and the load generator) ──

/// Client-side transport statistics (returned with the client).
#[derive(Debug, Default)]
pub struct UdpClientStats {
    /// The client's own reliable retransmissions (its control frames
    /// re-sent before their ACK arrived).
    pub retrans_out: u64,
    /// Duplicated inbound REL frames (the SERVER's retransmissions, as
    /// observed by this client).
    pub dup_in: u64,
    /// Inbound REL frames dropped on a full out-of-order window.
    pub oob_dropped: u64,
    /// Outbound REL frames given up (no ACK within RETRANSIT_MAX).
    pub gave_up: u64,
}

/// A rUDP client: the mirror image of the server's demux/writer, as one
/// task (the load generator's and the e2e tests' client is a plain task,
/// not an actor, so the whole session state is task-local).
///
/// The read loop's only awaited source is the socket read, bounded by
/// the smaller of the retransmit interval and the caller's window — the
/// same single-future idiom the server uses (no multiplexing).
pub struct UdpClient {
    sock: UdpSocket,
    peer: SocketAddr,
    established: bool,
    /// Inbound reliable state (server→client).
    in_expected: u32,
    in_oob: HashMap<u32, Vec<u8>>,
    /// Ordered inbound REL frames awaiting the caller (delivered in
    /// sequence order; RAW frames bypass it — the lossy band is
    /// unordered by design).
    pending: VecDeque<FrameBody>,
    /// Outbound reliable state (client→server).
    out_seq: u32,
    acked: u32,
    out_retransmit: VecDeque<(u32, Bytes, Instant)>,
    pub stats: UdpClientStats,
    buf: Vec<u8>,
    /// A RAW frame produced by `process_datagram`, awaiting hand-back to
    /// the awaiting loop (the lossy band is unordered: it does not wait
    /// for the pending REL frames).
    raw: Option<FrameBody>,
}

impl UdpClient {
    /// Connect: bind an ephemeral local port and run the stateless
    /// handshake (challenge → proof). Retries the challenge a few times
    /// (a lost challenge or proof is healed by the retry; the server is
    /// idempotent in it).
    pub async fn connect(addr: SocketAddr) -> std::io::Result<Self> {
        let sock = UdpSocket::bind(SocketAddr::from(([0, 0, 0, 0], 0)))
            .await
            .map_err(|e| std::io::Error::new(e.kind(), e.to_string()))?;
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0);
        let mut buf = vec![0u8; 2048];
        let challenge_deadline = Instant::now() + Duration::from_secs(3);
        let mut got_challenge = false;
        while !got_challenge && Instant::now() < challenge_deadline {
            let hello = encode_hello(nonce, 0);
            sock.send_to(&hello, addr).await?;
            match tokio::time::timeout(Duration::from_millis(500), sock.recv_from(&mut buf))
                .await
            {
                Ok(Ok((n, from))) if from == addr && n >= 18 && buf[0] == KIND_HELLO => {
                    let echoed = u64::from_le_bytes(buf[1..9].try_into().unwrap());
                    if echoed == nonce {
                        let cookie = u64::from_le_bytes(buf[9..17].try_into().unwrap());
                        let proof = encode_hello(nonce, cookie);
                        sock.send_to(&proof, addr).await?;
                        got_challenge = true;
                    }
                }
                Ok(Ok(_)) => {} // stray / wrong peer: ignore
                Ok(Err(e)) => return Err(e),
                Err(_) => continue, // no challenge yet: retry
            }
        }
        if !got_challenge {
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "rUDP handshake: no challenge within the deadline",
            ));
        }
        Ok(Self {
            sock,
            peer: addr,
            established: true,
            in_expected: 1,
            in_oob: HashMap::new(),
            pending: VecDeque::new(),
            out_seq: 0,
            acked: 1,
            out_retransmit: VecDeque::new(),
            stats: UdpClientStats::default(),
            buf: vec![0u8; 2048],
            raw: None,
        })
    }

    /// The server's address.
    /// One-shot liveness probe (the UDP readiness check a supervisor or
    /// orchestrator can use in place of a TCP connect probe — UDP has no
    /// SYN to probe with): send a challenge request on `sock` and wait up
    /// to `wait` for the challenge. A true answer means the demux is
    /// alive and answering; nothing is established (no session is
    /// created by a bare challenge request).
    pub async fn challenge_probe(sock: &UdpSocket, addr: SocketAddr, wait: Duration) -> bool {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0);
        if sock.send_to(&encode_hello(nonce, 0), addr).await.is_err() {
            return false;
        }
        let mut buf = [0u8; 32];
        tokio::time::timeout(wait, sock.recv_from(&mut buf))
            .await
            .is_ok_and(|r| {
                matches!(
                    r,
                    Ok((n, from)) if from == addr && n >= 18 && buf[0] == KIND_HELLO
                )
            })
    }

    pub fn peer(&self) -> SocketAddr {
        self.peer
    }

    /// The local bound address.
    pub fn local_addr(&self) -> Option<SocketAddr> {
        self.sock.local_addr().ok()
    }

    /// Whether the handshake completed.
    pub fn is_established(&self) -> bool {
        self.established
    }

    /// Send one application frame: control band (reliable, sequenced) or
    /// game band (RAW, loss-tolerant) — the same split as the server.
    pub async fn send_frame(&mut self, op: u16, payload: impl Into<Bytes>) -> std::io::Result<()> {
        let frame = FrameBody::new(op, payload.into());
        if is_control(op) {
            self.out_seq = self.out_seq.wrapping_add(1);
            let dg = Bytes::from(encode_rel(self.out_seq, &frame));
            self.out_retransmit
                .push_back((self.out_seq, dg.clone(), Instant::now()));
            self.sock.send_to(&dg, self.peer).await.map(|_| ())
        } else {
            let dg = encode_raw(&frame);
            self.sock.send_to(&dg, self.peer).await.map(|_| ())
        }
    }

    /// Receive one application frame, waiting up to `wait` for it. While
    /// waiting the loop performs the outbound retransmit pass (on the RTO
    /// ticks) — reliable delivery without any multiplexing.
    ///
    /// Returns `Ok(None)` when the window elapses with no frame (NOT an
    /// error: UDP has no EOF — the caller probes liveness explicitly when
    /// it needs the "session gone" distinction).
    pub async fn recv_frame(&mut self, wait: Duration) -> std::io::Result<Option<FrameBody>> {
        // A REL frame that is already ordered is returned immediately
        // (no wait, no syscall).
        if let Some(f) = self.pending.pop_front() {
            return Ok(Some(f));
        }
        let deadline = Instant::now() + wait;
        loop {
            let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                return Ok(None);
            };
            let w = remaining.min(RETRANSIT_RTO);
            match tokio::time::timeout(w, self.sock.recv_from(&mut self.buf)).await {
                Ok(Ok((n, from))) if from == self.peer => {
                    // Copy out first: process_datagram takes &mut self and
                    // must not borrow self.buf through the same call.
                    let data = self.buf[..n].to_vec();
                    self.process_datagram(&data);
                    if let Some(raw) = self.raw.take() {
                        // A RAW frame (the lossy band, unordered): return
                        // it immediately, without waiting for ordered REL.
                        return Ok(Some(raw));
                    }
                    if let Some(f) = self.pending.pop_front() {
                        return Ok(Some(f));
                    }
                }
                Ok(Ok(_)) => {} // stray peer: ignore
                Ok(Err(e)) => return Err(e),
                Err(_) => {
                    // RTO tick (or the window's edge): retransmit pass.
                    self.retransmit_pass();
                    if let Some(f) = self.pending.pop_front() {
                        return Ok(Some(f));
                    }
                    if Instant::now() >= deadline {
                        return Ok(None);
                    }
                }
            }
        }
    }
}

impl UdpClient {
    /// Handle one inbound datagram (from the server). Returns `true` when
    /// it produced a RAW frame (already in `self.raw`) so the awaiting
    /// loop can return it immediately; REL/ACK/HELLO return `false`.
    fn process_datagram(&mut self, d: &[u8]) -> bool {
        if d.is_empty() {
            return false;
        }
        match d[0] {
            KIND_RAW => {
                if let Some(fb) = body_of(&d[1..], 0) {
                    // Lossy band: no seq, no dedupe — hand it straight to
                    // the loop (unordered by design).
                    self.raw = Some(fb);
                    return true;
                }
                false
            }
            KIND_REL => {
                if d.len() < 7 {
                    return false;
                }
                let seq = u32::from_le_bytes(d[1..5].try_into().unwrap());
                let body = d[5..].to_vec();
                match seq.cmp(&self.in_expected) {
                    std::cmp::Ordering::Equal => {
                        if let Some(fb) = body_of(&body, 0) {
                            self.pending.push_back(fb);
                        }
                        self.in_expected = self.in_expected.wrapping_add(1);
                        while let Some(gap) = self.in_oob.remove(&self.in_expected) {
                            if let Some(fb) = body_of(&gap, 0) {
                                self.pending.push_back(fb);
                            }
                            self.in_expected = self.in_expected.wrapping_add(1);
                        }
                        let _ = self.sock.try_send_to(&encode_ack(self.in_expected), self.peer);
                    }
                    std::cmp::Ordering::Less => {
                        // The server retransmitted a frame already ACKed:
                        // count it (its retransmit, not our loss) and
                        // re-ACK; never re-deliver (control runs exactly
                        // once, in order).
                        self.stats.dup_in += 1;
                        let _ = self.sock.try_send_to(&encode_ack(self.in_expected), self.peer);
                    }
                    std::cmp::Ordering::Greater => {
                        if self.in_oob.len() < OOB_CAP {
                            self.in_oob.insert(seq, body);
                        } else {
                            self.stats.oob_dropped += 1;
                        }
                        // No ACK for a gap: the missing frame is still
                        // outstanding server-side and will be re-sent.
                    }
                }
                false
            }
            KIND_ACK => {
                if d.len() < 5 {
                    return false;
                }
                let ack = u32::from_le_bytes(d[1..5].try_into().unwrap());
                self.acked = self.acked.max(ack);
                while let Some(&(s, _, _)) = self.out_retransmit.front() {
                    if s < self.acked {
                        self.out_retransmit.pop_front();
                    } else {
                        break;
                    }
                }
                false
            }
            // The server only sends HELLO during the handshake (already
            // complete here): ignore any late one.
            _ => false,
        }
    }

    /// Retransmit the oldest un-ACKed outbound control frame whose RTO
    /// has passed; give up (counted) on frames older than RETRANSIT_MAX.
    fn retransmit_pass(&mut self) {
        let now = Instant::now();
        while let Some((_, datagram, sent)) = self.out_retransmit.front_mut() {
            if *sent + RETRANSIT_MAX <= now {
                self.out_retransmit.pop_front();
                self.stats.gave_up += 1;
                continue;
            }
            if *sent + RETRANSIT_RTO <= now {
                match self.sock.try_send_to(datagram, self.peer) {
                    Ok(_) => {
                        *sent = now;
                        self.stats.retrans_out += 1;
                    }
                    Err(_) => break, // socket busy: retry next tick
                }
            } else {
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use tokio::sync::mpsc;

    /// Bind a transport and run an accept loop that hands endpoints to an
    /// unbounded channel (the unit-test stand-in for the server's accept
    /// loop).
    async fn bound_transport(
        config: UdpTransportConfig,
    ) -> (
        Arc<dyn Listener>,
        std::net::SocketAddr,
        mpsc::UnboundedReceiver<Endpoint>,
        tokio::task::JoinHandle<()>,
    ) {
        let transport = Arc::new(UdpTransport { config });
        let listener = transport
            .bind("127.0.0.1:0".parse().unwrap())
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("local addr");
        let (ep_tx, ep_rx) = mpsc::unbounded_channel();
        let l2 = Arc::clone(&listener);
        let accept = tokio::spawn(async move {
            loop {
                let l = Arc::clone(&l2);
                match l.accept().await {
                    Ok(ep) => {
                        if ep_tx.send(ep).is_err() {
                            break;
                        }
                    }
                    Err(_) => break, // listener closed
                }
            }
        });
        (listener, addr, ep_rx, accept)
    }

    /// Direct-demux harness: a demux on a bound socket with one session
    /// pre-installed (handshake skipped), its inbound mailbox exposed.
    fn demux_with_session(
        sock: Arc<UdpSocket>,
        peer: SocketAddr,
    ) -> (Demux, gsb_core::channel::Inbox<gsb_core::conn::ConnIn>) {
        let (end_tx, _end_rx) = crossbeam_channel::bounded(4);
        let (in_tx, in_rx) = gsb_core::channel::channel(16);
        let (out_tx, _out_rx) = gsb_core::channel::channel(16);
        let mut d = Demux {
            sock,
            end_tx,
            cookie: CookieKey::generate().expect("OS entropy in test"),
            inbox_cap: 16,
            outbox_cap: 16,
            max_datagram: DEFAULT_MAX_DATAGRAM_BYTES,
            idle: None,
            sessions: HashMap::new(),
            deadlines: BTreeSet::new(),
            buf: vec![0u8; 65536],
            established: 1,
            challenges: 0,
            bad_cookie: 0,
            endpoints_dropped: 0,
            swept_idle: 0,
            removed_actor_gone: 0,
            acks_piggybacked: 0,
            ack_piggyback_failed: 0,
            oversized_in: 0,
            bad_datagrams: 0,
        };
        d.sessions.insert(
            peer,
            UdpSession {
                in_tx,
                out_tx,
                last_seen: Instant::now(),
                in_expected: 1,
                in_oob: HashMap::new(),
                oob_dropped: 0,
                dup_in: 0,
                inbox_full: 0,
                inbox_full_warned: false,
            },
        );
        (d, in_rx)
    }

    /// Feed one crafted datagram to the demux (bypassing the socket).
    fn feed(d: &mut Demux, peer: SocketAddr, datagram: &[u8]) {
        let n = datagram.len().min(d.buf.len());
        d.buf[..n].copy_from_slice(&datagram[..n]);
        d.handle(n, peer);
    }

    fn rel_frame(seq: u32, op: u16, payload: &[u8]) -> Vec<u8> {
        encode_rel(seq, &FrameBody::new(op, Bytes::copy_from_slice(payload)))
    }

    /// The two-phase topology works: sequential handshakes each produce an
    /// endpoint (with the correct peer), and the per-session writer
    /// delivers a control frame to the client's reliable band.
    #[tokio::test]
    async fn sequential_handshakes_and_writer_roundtrip() {
        let (listener, addr, mut eps, _accept) =
            bound_transport(UdpTransportConfig::default()).await;

        let a = UdpClient::connect(addr).await.expect("A handshake");
        let ep_a = tokio::time::timeout(Duration::from_secs(3), eps.recv())
            .await
            .expect("A endpoint")
            .expect("A endpoint");
        assert_eq!(ep_a.peer().unwrap().port(), a.local_addr().unwrap().port());

        let mut b = UdpClient::connect(addr).await.expect("B handshake");
        let mut ep_b = tokio::time::timeout(Duration::from_secs(3), eps.recv())
            .await
            .expect("B endpoint")
            .expect("B endpoint");
        assert_eq!(ep_b.peer().unwrap().port(), b.local_addr().unwrap().port());

        // Drive B's outbound path: pump + a control frame.
        let (in_tx, _in_rx) = ep_b.take_inbox(16);
        let (out_tx, out_rx) = ep_b.take_outbox(16);
        let (_reader, _writer) = ep_b.start_pump(ConnectionId(2), in_tx, out_rx, None);
        let fb = FrameBody::new(gsb_protocol::op::base::HEARTBEAT_ACK, Bytes::from(vec![7, 9]));
        out_tx.send(vec![fb]).await.expect("send to writer");
        let got = tokio::time::timeout(Duration::from_secs(3), b.recv_frame(Duration::from_millis(1000)))
            .await
            .expect("B recv window")
            .expect("B recv")
            .expect("no frame for B");
        assert_eq!(got.op, gsb_protocol::op::base::HEARTBEAT_ACK);
        assert_eq!(got.payload.as_ref(), &[7u8, 9]);

        // The handshake must be stateless-per-peer: A is unaffected.
        let _ = a.local_addr();
        drop(b);
        listener.close();
    }

    /// A forged cookie (wrong proof) is rejected: no session, no
    /// endpoint — and the rejection leaves other clients unharmed.
    #[tokio::test]
    async fn forged_proof_is_rejected() {
        let (_listener, addr, mut eps, _accept) =
            bound_transport(UdpTransportConfig::default()).await;

        let raw = UdpSocket::bind("0.0.0.0:0".parse::<SocketAddr>().unwrap())
            .await
            .expect("raw client binds");
        let mut buf = vec![0u8; 2048];
        let nonce = 0xDEAD_BEEF_CAFE_F00Du64;

        // Challenge request.
        raw.send_to(&encode_hello(nonce, 0), addr)
            .await
            .expect("send challenge request");
        let (n, from) = tokio::time::timeout(Duration::from_secs(3), raw.recv_from(&mut buf))
            .await
            .expect("challenge arrives")
            .expect("recv");
        assert_eq!(from, addr);
        assert_eq!(buf[0], KIND_HELLO);
        let cookie = u64::from_le_bytes(buf[9..17].try_into().unwrap());
        assert!(cookie != 0, "the challenge must carry a real cookie");

        // Forged proof: cookie + 1. The demux cannot match it against
        // F(nonce, peer, key) — no session may be created.
        raw.send_to(&encode_hello(nonce, cookie.wrapping_add(1)), addr)
            .await
            .expect("send forged proof");
        assert!(
            tokio::time::timeout(Duration::from_millis(400), eps.recv())
                .await
                .is_err(),
            "a forged proof must not produce an endpoint"
        );

        // The rejection is scoped to the attacker: a real client still
        // gets a session.
        let c = UdpClient::connect(addr).await.expect("real client still works");
        let _ep = tokio::time::timeout(Duration::from_secs(3), eps.recv())
            .await
            .expect("real endpoint")
            .expect("real endpoint");
        assert_eq!(_ep.peer().unwrap().port(), c.local_addr().unwrap().port());
        let _ = n;
    }

    /// Idle teardown (item: no FIN in UDP — the previous turn's
    /// `idle_timeout` must work): a silent client's session is swept and
    /// its actor gets `ConnIn::ServerClosed` on its inbound channel.
    #[tokio::test]
    async fn idle_sweep_delivers_server_closed() {
        let cfg = UdpTransportConfig {
            idle_timeout: Some(Duration::from_millis(200)),
            ..Default::default()
        };
        let (_listener, addr, mut eps, _accept) = bound_transport(cfg).await;

        let client = UdpClient::connect(addr).await.expect("handshake");
        let mut ep = tokio::time::timeout(Duration::from_secs(3), eps.recv())
            .await
            .expect("endpoint")
            .expect("endpoint");
        let (_in_tx, mut in_rx) = ep.take_inbox(16);
        // The client stays connected but SILENT: the handshake datagrams
        // are its last inbound traffic. The demux deadline (200 ms) must
        // fire and remove the session.
        let closed = tokio::time::timeout(Duration::from_secs(3), in_rx.recv())
            .await
            .expect("the sweep must deliver within 3 s")
            .expect("the sweep must deliver an item");
        match closed {
            gsb_core::conn::ConnIn::ServerClosed { .. } => {}
            other => panic!("expected ServerClosed, got {other:?}"),
        }
        drop(client);
    }

    /// Inbound reliable band: an out-of-order datagram is buffered behind
    /// the gap and delivered in ORDER once the gap fills; a duplicate is
    /// re-ACKed but never re-forwarded.
    #[tokio::test]
    async fn inbound_rel_reorders_and_dedupes() {
        let sock = Arc::new(
            UdpSocket::bind("127.0.0.1:0".parse::<SocketAddr>().unwrap())
                .await
                .expect("bind"),
        );
        let peer = "127.0.0.1:9".parse().unwrap();
        let (mut d, mut in_rx) = demux_with_session(sock, peer);

        // seq 2 arrives FIRST (the gap): buffered, nothing forwarded, no
        // ACK advance.
        feed(&mut d, peer, &rel_frame(2, 1000, b"second"));
        assert!(in_rx.is_empty(), "a gapped frame must wait for order");

        // seq 1 fills the gap: BOTH frames flow, in order.
        feed(&mut d, peer, &rel_frame(1, 1000, b"first"));
        let f1 = in_rx.try_recv().expect("first frame");
        let f2 = in_rx.try_recv().expect("second frame");
        assert!(in_rx.is_empty());
        match (&f1, &f2) {
            (
                gsb_core::conn::ConnIn::Frame(a),
                gsb_core::conn::ConnIn::Frame(b),
            ) => {
                assert_eq!(a.payload.as_ref(), b"first");
                assert_eq!(b.payload.as_ref(), b"second");
            }
            _ => panic!("expected two frames"),
        }

        // Duplicate seq 1: re-ACKed (the demux would have sent an ACK
        // datagram on the socket — not observable here), but NOT
        // re-forwarded.
        feed(&mut d, peer, &rel_frame(1, 1000, b"first"));
        assert!(in_rx.is_empty(), "duplicates must never be re-forwarded");
    }

    /// MTU policy: an outbound datagram over the budget is dropped and
    /// counted (never fragmented, v1 constraint) while smaller frames on
    /// the same session — control AND game band — are still delivered.
    #[tokio::test]
    async fn oversized_outbound_is_dropped_not_fragmented() {
        let cfg = UdpTransportConfig {
            max_datagram_bytes: 40,
            ..Default::default()
        };
        let (_listener, addr, mut eps, _accept) = bound_transport(cfg).await;

        let mut client = UdpClient::connect(addr).await.expect("handshake");
        let mut ep = tokio::time::timeout(Duration::from_secs(3), eps.recv())
            .await
            .expect("endpoint")
            .expect("endpoint");
        let (in_tx, _in_rx) = ep.take_inbox(16);
        let (out_tx, out_rx) = ep.take_outbox(16);
        let (_r, _w) = ep.start_pump(ConnectionId(1), in_tx, out_rx, None);

        // RAW band, 25-byte payload → 28-byte datagram: fits.
        out_tx
            .send(vec![FrameBody::new(
                1000,
                Bytes::copy_from_slice(&[1u8; 25]),
            )])
            .await
            .unwrap();
        let ok = tokio::time::timeout(Duration::from_secs(3), client.recv_frame(Duration::from_millis(200)))
            .await
            .expect("window")
            .expect("recv")
            .expect("the in-budget frame must arrive");
        assert_eq!(ok.payload.len(), 25);

        // RAW band, 40-byte payload → 43-byte datagram: over the 40-byte
        // budget. Dropped, not fragmented.
        out_tx
            .send(vec![FrameBody::new(
                1000,
                Bytes::copy_from_slice(&[2u8; 40]),
            )])
            .await
            .unwrap();

        // A control frame (small) is still delivered on the same session.
        out_tx
            .send(vec![FrameBody::new(
                gsb_protocol::op::base::HEARTBEAT_ACK,
                Bytes::from(vec![1, 2]),
            )])
            .await
            .unwrap();
        let ctl = tokio::time::timeout(Duration::from_secs(3), client.recv_frame(Duration::from_millis(1000)))
            .await
            .expect("window")
            .expect("recv")
            .expect("control must still be delivered");
        assert_eq!(ctl.op, gsb_protocol::op::base::HEARTBEAT_ACK);

        // The oversized frame must NOT arrive (neither whole nor
        // fragmented): the next frame is the control one.
        let next = tokio::time::timeout(
            Duration::from_millis(600),
            client.recv_frame(Duration::from_millis(400)),
        )
        .await
        .expect("window")
        .expect("recv");
        assert!(next.is_none(), "the oversized frame must be dropped, not delivered");
    }

    /// Reliability, server side: a control frame the client never ACKs is
    /// retransmitted on the RTO (the raw client observes the same seq
    /// again with an identical payload).
    #[tokio::test]
    async fn server_retransmits_until_ack() {
        let (_listener, addr, mut eps, _accept) =
            bound_transport(UdpTransportConfig::default()).await;

        // Raw client (full ACK control).
        let raw = UdpSocket::bind("0.0.0.0:0".parse::<SocketAddr>().unwrap())
            .await
            .expect("bind");
        let mut buf = vec![0u8; 2048];
        let nonce = 0x0123_4567_89AB_CDEFu64;
        raw.send_to(&encode_hello(nonce, 0), addr)
            .await
            .unwrap();
        let _ = tokio::time::timeout(Duration::from_secs(3), raw.recv_from(&mut buf))
            .await
            .expect("challenge")
            .expect("recv");
        let cookie = u64::from_le_bytes(buf[9..17].try_into().unwrap());
        raw.send_to(&encode_hello(nonce, cookie), addr)
            .await
            .unwrap();

        // Fake actor: one control frame on the outbound channel.
        let mut ep = tokio::time::timeout(Duration::from_secs(3), eps.recv())
            .await
            .expect("endpoint")
            .expect("endpoint");
        let (in_tx, _in_rx) = ep.take_inbox(16);
        let (out_tx, out_rx) = ep.take_outbox(16);
        let (_r, _w) = ep.start_pump(ConnectionId(1), in_tx, out_rx, None);
        out_tx
            .send(vec![FrameBody::new(
                gsb_protocol::op::base::ERROR,
                Bytes::from(vec![9, 0]),
            )])
            .await
            .unwrap();

        // First delivery: REL seq 1.
        let (n1, _) = tokio::time::timeout(Duration::from_secs(3), raw.recv_from(&mut buf))
            .await
            .expect("first delivery")
            .expect("recv");
        let first = buf[..n1].to_vec();
        assert_eq!(first[0], KIND_REL);
        let seq1 = u32::from_le_bytes(first[1..5].try_into().unwrap());
        assert_eq!(seq1, 1);

        // No ACK. The RTO (50 ms) must retransmit the SAME frame.
        let (n2, _) = tokio::time::timeout(Duration::from_secs(3), raw.recv_from(&mut buf))
            .await
            .expect("retransmit must arrive")
            .expect("recv");
        let second = buf[..n2].to_vec();
        assert_eq!(first, second, "the retransmit is byte-identical (same seq, same payload)");
    }

    // ── cookie key: entropy source (this turn) ───────────────────────

    /// Reference of the REMOVED v1 derivation (wall-clock nanoseconds),
    /// kept only as a regression oracle: a key equal to `legacy(t)` for
    /// some instant `t` is computable from the server start time — the
    /// property this turn removes.
    fn legacy_time_key(nanos_since_epoch: u64) -> CookieKey {
        let mut a = nanos_since_epoch;
        let mut b = a.rotate_left(13) ^ (a >> 21);
        sm64(&mut a);
        sm64(&mut b);
        CookieKey(a | 1, b | 2)
    }

    /// The key is no longer a function of the wall clock: capture the
    /// clock, generate, capture again, and enumerate the legacy
    /// derivation over the EXACT [t0, t1] window at every plausible time
    /// granularity (ns, µs, ms). A time-derived key must land on an
    /// enumerated value; an OS-entropy draw cannot (a collision with any
    /// one candidate is 2^-128, the window holds ≤ ~10^5 of them).
    #[test]
    fn cookie_key_is_not_derived_from_the_wall_clock() {
        let now_ns =
            || std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).expect("clock").as_nanos();
        let t0 = now_ns();
        let key = CookieKey::generate().expect("OS entropy in test");
        let t1 = now_ns();
        assert!(t1 >= t0, "the window must not be empty");
        for unit in [1u128, 1_000, 1_000_000] {
            for t in (t0 / unit)..=(t1 / unit) {
                assert_ne!(
                    key,
                    legacy_time_key(t as u64),
                    "key equals the legacy wall-clock derivation at t={t}"
                );
            }
        }
    }

    /// The config path is pure: the same bytes give the same key, and F
    /// is deterministic for a fixed (nonce, peer) — an operator-supplied
    /// key makes the handshake reproducible (and auditable) by
    /// construction.
    #[test]
    fn cookie_key_from_bytes_is_deterministic() {
        let peer: SocketAddr = "127.0.0.1:1234".parse().unwrap();
        let other: SocketAddr = "127.0.0.1:1235".parse().unwrap();
        let a = CookieKey::from_bytes([1u8; 16]);
        let b = CookieKey::from_bytes([1u8; 16]);
        assert_eq!(a, b, "the config path must be a pure function of the bytes");
        assert_eq!(a.compute(42, peer), b.compute(42, peer));
        assert_ne!(a.compute(42, peer), a.compute(43, peer));
        assert_ne!(a.compute(42, peer), a.compute(42, other));
    }

    /// The OS-entropy source is non-degenerate: two consecutive draws
    /// differ (a collision is 2^-128 — a match means the source is
    /// constant or broken, not bad luck).
    #[test]
    fn cookie_key_generate_draws_distinct_keys() {
        let a = CookieKey::generate().expect("OS entropy in test");
        let b = CookieKey::generate().expect("OS entropy in test");
        assert_ne!(a, b, "two consecutive OS-entropy draws collided");
    }
}
