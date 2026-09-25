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
//! server → ACK { 1 }                          (accept: "send me seq 1")
//! ```
//!
//! `F` is a per-process-keyed mix (splitmix64 folds of `nonce`, the peer
//! address, the key and the time slot) — not a KDF: v1 has no crypto
//! layer (out of scope), and the property needed is "unforgeable over the
//! network without the key". The challenge response is well-formed only
//! for a well-formed challenge (same 18-byte size), so forged-traffic
//! amplification stays at ratio ≤ 1, and a forged proof needs the
//! cookie, which needs the key. The accept (5 bytes) answers only a
//! proof that verifies, so it is owed only to an address that received
//! its challenge: ratio 5/18, and nothing at all for a forged proof.
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
//!   18 bytes each way (the accept that "Handshake loss" added later is
//!   a separate datagram, not a change to these). The slot is not sent: both sides of the server's
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
//!   a legitimate handshake never loses a race with the rotation: the
//!   client re-sends a lost proof for at most `HANDSHAKE_DEADLINE`
//!   (5 s, pinned below one slot at compile time), so even its last
//!   re-send carries a cookie the server still accepts (see "Handshake
//!   loss");
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
//! ## Handshake loss (why `connect` waits for the accept)
//!
//! Every datagram of the handshake can be lost, and before this section
//! existed one of them could not be healed. The client re-sent its
//! challenge request (every 500 ms, for 3 s) but counted itself
//! connected the moment it had SENT its proof. A proof lost on the way —
//! measured: 200+ clients handshaking at once overflow the one server
//! socket's receive queue on loopback — left a client that believed in a
//! session the server never created. Its AUTH found nothing, its REL
//! band died 5 s later, and at afe7fba only 65-101 of 200 simultaneous
//! loadgen clients ever joined. Loss points, before → after:
//!
//! | lost datagram | before | now |
//! |---|---|---|
//! | challenge request / challenge | re-requested (500 ms) | re-requested (`HANDSHAKE_RTO`) |
//! | proof | **never healed** (client "connected", no session) | proof re-sent until the accept |
//! | accept | — (did not exist) | proof re-sent; the server answers again |
//! | first control frame (AUTH) | REL retransmit | REL retransmit (unchanged) |
//!
//! **Decision: the client is established only on the server's word.**
//! After a proof that verifies (and whose endpoint reached the accept
//! loop) the server sends the **accept**: an `ACK { 1 }` — the new
//! session's cumulative ACK, "send me seq 1", an existing datagram kind
//! that a client of any age treats as a no-op. [`UdpClient::connect`]
//! returns only once the server has shown it holds the session: the
//! accept, or ANY session datagram (ACK/REL/RAW/FRAG, which the server
//! sends only to a peer in its session table — one that arrives in place
//! of a lost accept is handed to the inbound path, not swallowed).
//! Until then it re-sends the current step — challenge request or proof —
//! every `HANDSHAKE_RTO` (the transport's one RTO, 50 ms), and gives up
//! with `TimedOut` at `HANDSHAKE_DEADLINE` (the REL liveness bound,
//! 5 s: "the server answered nothing for 5 s" means the same before a
//! session exists as after). The re-sends are counted
//! ([`UdpClientStats::challenge_retries`], [`UdpClientStats::proof_retries`];
//! loadgen `hs_retries`).
//!
//! **The server is idempotent in the proof.** A valid proof from an
//! address that already has a session is a RE-SEND (the first copy's
//! accept was lost, or the proof was duplicated in flight): it is
//! answered with the session's CURRENT cumulative ACK and nothing else —
//! no second session, no second `ConnectionId`, the reliable state not
//! reset. A challenge request or an unverifiable proof from that address
//! is still answered with nothing, so an established session is not a
//! reflector.
//!
//! **The rotation argument.** Every proof re-send reuses the FIRST
//! cookie (a second challenge is ignored). That cookie was minted in
//! some slot N after the client's first request left, and it verifies
//! until slot N+1 ends — at least one `COOKIE_SLOT` (10 s) later. The
//! last re-send leaves at most 5 s after the first request, so it lands
//! inside that window on any path whose one-way delay is under the
//! other 5 s: a proof retried across a rotation validates, and no
//! restart path is needed. The inequality is a compile-time assertion;
//! lengthening the deadline past a slot breaks the build, not a
//! handshake.
//!
//! **The cost, pinned.** An uneventful handshake is one datagram longer:
//! HELLO 18 B → challenge 18 B → proof 18 B → **accept 5 B**, and
//! `connect` returns one RTT later than it used to (the client's AUTH
//! now waits for the accept). That is the price of the handshake being
//! self-contained — the same 2-RTT shape as QUIC's Retry path and DTLS's
//! HelloVerifyRequest, the two stateless-cookie handshakes this one
//! resembles. A give-up leaves no zombie: a proof that never landed
//! allocated nothing, and a session whose every accept was lost sees no
//! further traffic and is ended by the idle sweep.
//!
//! **Rejected — raising `SO_RCVBUF`** (BACKLOG B4, the "No receive-buffer
//! tuning" item below): it moves the loss threshold, it does not remove
//! it. A deeper queue absorbs 500 handshakes but not 5 000, and a lossy
//! real path drops a proof regardless of the server's buffer; a
//! handshake that cannot heal one lost datagram is wrong at any queue
//! depth. It stays open as a throughput knob, not as this fix.
//! **Rejected — server-side pacing of handshakes** (admit N per tick,
//! drop or defer the rest): the server cannot pace what the kernel has
//! already dropped before the demux saw it, and deferring means holding
//! state for unverified peers — exactly what the stateless handshake
//! forbids. **Rejected — confirm on the first server datagram without an
//! accept** (no wire change): the server sends nothing until the
//! client's first control frame, so `connect` would return an
//! unconfirmed session, the proof's re-sends would ride the REL band's
//! clock, and a client that stays silent could never learn whether it is
//! connected. **Rejected — the client's frames carry the cookie** (TCP
//! SYN-cookie style, any frame re-validates): 8 bytes on every datagram
//! of the session to save one 5-byte datagram per session.
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
//!   4 FRAG   [u16 LE msg id][u8 index][u8 count][chunk]
//!                                               game band over the budget,
//!                                               server → client only
//! ```
//!
//! The band split is by opcode: `op <= 64` (base control band) is
//! REL, `op >= 1000` (game band) is RAW — or FRAG when one RAW datagram
//! would exceed the budget (see "MTU").
//!
//! **Status: experimental (v1).** Validated on loopback and by the e2e
//! suite; NOT hardened for lossy real-world networks. Two correctness
//! gaps this paragraph used to name are now **closed**: the silent REL
//! give-up (see "The REL liveness bound") and the handshake cookie that
//! never expired (see "Cookie rotation"). The label stays, because what
//! is missing is not a bug list but a body of work — see "What is still
//! open" at the end of this doc. Production deployments should still run
//! a hardened transport behind the same [`crate::Transport`] seam;
//! graduating this one is a product decision, not a code change.
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
//! ## MTU (feature 3): the game band fragments
//!
//! The datagram budget (`max_datagram_bytes`, default 1472 =
//! MTU 1500 − IP 20 − UDP 8) holds for every datagram. A **game-band**
//! frame whose RAW datagram would exceed it is **fragmented** by the
//! session's writer and reassembled by the client (`frag`); every other
//! datagram — RAW within the budget, REL, ACK, HELLO — keeps its exact
//! bytes. The unit is the frame, whatever it carries: a group snapshot
//! (delta or full), a keep-alive full, the one-shot `Private` full. So
//! the core's one-payload-per-group contract and the kit's envelope are
//! untouched: fragmentation is invisible above the transport.
//!
//! **Why here.** Measured before this change (afe7fba, loopback): the
//! arena's team-fog fulls reach 1.9 KB at 200 units and 5.1 KB at 500;
//! the MMO's clustered load sends 1.1-2.9 KB *deltas* as well as fulls.
//! On TCP that is one frame; here the writer used to drop every one of
//! them (arena 500: 93 % of snapshot datagrams). The largest single
//! entity record is 17 bytes, so a message always splits cleanly.
//!
//! - **Wire.** `4 FRAG [u16 msg id][u8 index][u8 count][chunk]`: the
//!   chunks, in index order, are the RAW datagram's bytes after its kind
//!   byte (`[u16 op][payload]`). Equal chunks of `budget − 5` bytes, the
//!   last shorter. The id is per session, wrapping, compared in serial
//!   order. A client that predates FRAG ignores kind 4 — for it the
//!   frame is lost exactly as it was before.
//! - **Loss.** A message is delivered only when every fragment arrived;
//!   there is no retransmission. A message missing a fragment is dropped
//!   (counted) when a newer message takes its slot or when it is older
//!   than [`frag::FRAG_MAX_AGE`] (250 ms); its stragglers are refused,
//!   never resurrected. The band is self-healing as before: the next
//!   full (at the latest the keep-alive) replaces a lost one.
//! - **Bounds (client, all constant, O(1) per datagram):**
//!   [`frag::FRAG_MAX_COUNT`] = 16 fragments per message (23 472 bytes at
//!   the default budget — 2.3× the largest measured full, arena 1000's
//!   10 267; a frame past it takes the old drop+count path, warned once
//!   per session); [`frag::FRAG_SLOTS`] = 4 messages under reassembly
//!   (slot = id mod 4, so a newer message evicts only its slot's older
//!   partial; one tick ships at most two fragmented messages per session
//!   — the group frame and the private full); [`frag::FRAG_MEM_CAP`] =
//!   64 KiB of held chunks per session (over it, the oldest OTHER
//!   partial is evicted). No map, no growth.
//! - **The control band never fragments.** Its server → client frames
//!   (AUTH/JOIN/LEAVE results, HEARTBEAT_ACK, ERROR) are tens of bytes;
//!   the only variable fields echo what the client itself sent in one
//!   in-budget datagram. A control frame over the budget is therefore a
//!   bug, and it is **session-fatal** (the reliable band's death path),
//!   checked before a seq is spent — dropping it after taking a seq, as
//!   before, wedged the peer's cumulative stream and surfaced 5 s later
//!   as a misattributed no-ACK death.
//! - **Client → server fragments are refused** (counted by the demux,
//!   nothing forwarded). Inputs are tens of bytes; reassembly on the
//!   server would be memory any session could make it hold, in the one
//!   task every session shares.
//!
//! **Counters.** Writer (per session, logged at its end):
//! `frag_messages`, `frag_datagrams`, `dropped_oversized` (past the
//! ceiling). Client ([`UdpClientStats`]): `frag_reassembled`,
//! `frag_dropped_incomplete`, `frag_rejected`. The room's
//! `max_snapshot_bytes` / `snap_overflows` keep their meaning (payloads
//! over the size) but on rUDP they are now a bandwidth/fragmentation
//! signal, not a loss signal: the loss signal is
//! `frag_dropped_incomplete`.
//!
//! **Rejected — a kit-level multi-part snapshot** (the kit splits a group
//! frame into self-describing parts, the client reassembles): the same
//! loss behaviour, but it changes the core seam (one payload per group
//! becomes several), adds protocol fields to the envelope, changes every
//! client — and covers only the frames the kit builds, not the one-shot
//! `Private` full or a game's own large frame. **Rejected — the old
//! advice, "split the snapshot group instead":** it remains the right
//! bandwidth answer, but as the only answer it made rUDP unusable for
//! the arena's team fog and the MMO's clustered crowds, where the group
//! IS the game rule. **Rejected — relying on IP fragmentation** (send
//! the whole datagram, let the kernel split it): an IPv4 fragment loss
//! loses the datagram just the same, IPv6 routers never fragment, and
//! many middleboxes drop IP fragments outright. **Rejected — reliable
//! fragments** (ACK + retransmit per fragment): the band is
//! self-healing; a retransmitted old snapshot is worth less than the
//! next one.

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
//!
//! ## What is still open (why the label stays)
//!
//! Each of these was re-verified against the code at the time this list
//! was written; none of them is a one-line fix, and each is the kind of
//! thing a hardened transport ships with.
//!
//! - **No congestion control and no pacing.** Nothing in this module
//!   limits the rate at which a session's writer puts datagrams on the
//!   socket: the only thing bounding server pps is the room's own tick
//!   and snapshot budget. On loopback that is invisible; on a real
//!   network a room fan-out plus retransmissions can push a slow path
//!   into a loss spiral it has no way to back out of. (Verified: no
//!   token bucket, no pacer, no window anywhere under `udp/`.)
//! - **Fixed RTO, no RTT estimation.** [`RETRANSIT_RTO`] is a compile-time
//!   50 ms for every peer on earth, and it never backs off. A 200 ms path
//!   therefore gets ~4 redundant copies of every control frame before the
//!   first ACK can possibly arrive — wasteful in the good case and
//!   actively harmful in the loss case, which is exactly why the
//!   backoff-shaped alternative was rejected for the liveness bound until
//!   this exists. (Verified: no RTT sample is taken; the constant is used
//!   as-is by both the writer and the client.)
//! - **NAT rebinding ends the session.** Sessions are keyed by the
//!   peer's 4-tuple, so a rebind is a new address: a new handshake, a new
//!   `ConnectionId`, and the old session lingering until the idle sweep.
//!   A mobile client that changes network loses its session where QUIC
//!   would migrate it. Fixing this needs a connection id on the wire and
//!   an identity to re-bind it to — a protocol change plus the auth
//!   layer, not a patch. (Verified: `Demux::sessions` is keyed by
//!   `SocketAddr` and `handle_hello` early-returns for a known one.)
//! - **No receive-buffer tuning.** ONE socket carries every session, so
//!   its kernel receive queue is the first and only buffer under a burst,
//!   and it is left at the system default: `tokio::net::UdpSocket`
//!   (1.53.1) exposes no `SO_RCVBUF` setter, so raising it needs the raw
//!   fd. (Verified: the only mention is the note in `bind`.)
//! - **No crypto layer**, declared out of scope for v1 rather than
//!   pending: the cookie is an anti-spoofing measure, not a security
//!   boundary (nothing is signed or encrypted). Fragmentation, once
//!   listed here, now exists for the game band (see "MTU"); a FRAG
//!   datagram is as forgeable as a RAW one, and the client's reassembly
//!   bounds are what keep a forged stream from costing it more than
//!   64 KiB.
//! - **The handshake re-sends on the same fixed RTO.** A lost proof is
//!   healed now (see "Handshake loss"), but on a path whose RTT exceeds
//!   50 ms the client sends several copies of each handshake step before
//!   the first answer can arrive — the handshake's share of the "Fixed
//!   RTO" item above (each copy is answered at ratio ≤ 1).

mod client;
mod cookie;
mod demux;
mod frag;
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
use frag::{FRAG_MAX_COUNT, Reassembly, split};
use wire::{body_of, encode_ack, encode_hello, encode_raw, encode_rel};
use writer::udp_pump_spawner;

use std::time::Duration;

use gsb_protocol::op;

/// Datagram kinds (see module docs).
pub const KIND_RAW: u8 = 0;
pub const KIND_REL: u8 = 1;
pub const KIND_ACK: u8 = 2;
pub const KIND_HELLO: u8 = 3;
/// A fragment of an over-budget game-band frame (server → client only;
/// see "MTU (feature 3)").
pub const KIND_FRAG: u8 = 4;

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
/// The client's handshake re-send interval, for the challenge request
/// and the proof alike (see the module docs, "Handshake loss"): a step
/// that has had no answer for this long is sent again. The transport's
/// one RTO — measured against 250 ms at 500 simultaneous handshakes, it
/// cut the connect p50 from ~250 ms to ~50 ms with no more re-sends.
const HANDSHAKE_RTO: Duration = RETRANSIT_RTO;
/// How long the client keeps re-sending before the handshake gives up
/// with `TimedOut`: the REL liveness bound, so "the server answered
/// nothing for 5 s" means the same thing before a session exists as it
/// does after.
const HANDSHAKE_DEADLINE: Duration = REL_NO_ACK_FATAL;
// Every proof re-send reuses the first cookie, and a cookie stays valid
// for at least one whole slot after it was minted: a give-up bound
// shorter than one slot means no retry can meet an expired cookie
// (module docs, "Handshake loss", the rotation argument).
const _: () = assert!(HANDSHAKE_DEADLINE.as_millis() < COOKIE_SLOT.as_millis());
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
