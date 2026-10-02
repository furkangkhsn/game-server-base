//! The `[[listeners]]` grammar: one entry per door.

use crate::config::*;

mod afk_action;
mod detach_hold;
mod room;
pub use room::RoomOverride;
pub(crate) use room::{GameDefaults, RoomTemplate};

/// The per-listener transport spelling inside a `[[listeners]]` entry.
///
/// WHY a separate enum from [`TransportKind`] instead of a `Tls` variant
/// there: TLS is NOT a distinct wire framing — it is TCP with a rustls
/// upgrade (the accept loop, pumps and actors cannot tell them apart), so
/// the legacy scalar key keeps encoding it as `transport = "tcp"` plus the
/// cert/key pair. A listener ARRAY needs to name the *deployments*
/// unambiguously in one key ("tls"/"quic" carry their own cert/key paths per
/// entry), and reusing `TransportKind` would silently widen the legacy
/// scalar grammar (`transport = "tls"` would start parsing where it used
/// to be a config error). Two small enums keep each grammar exactly as
/// wide as it was. The same argument keeps "quic"/"ws" OUT of the legacy
/// scalar grammar: they are array-only spellings, so an old config can
/// never change meaning under a new parser.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ListenerTransport {
    /// Length-prefixed plaintext TCP.
    Tcp,
    /// The same framing over rustls; the entry MUST set both
    /// `tls_cert` and `tls_key`.
    Tls,
    /// rUDP (one socket + one demux PER udp listener; see `gsb_net::udp`).
    Udp,
    /// QUIC over one UDP socket (quinn): each connection carries exactly
    /// ONE bidirectional stream framed like TCP (`gsb_net::quic`). The
    /// entry MUST set both `tls_cert` and `tls_key` — QUIC mandates
    /// TLS 1.3, and this is the SAME PEM pair the "tls" door loads (a
    /// deployment serves one identity through both doors).
    Quic,
    /// WebSocket: RFC 6455 upgrade over plain TCP; every binary message
    /// carries exactly one length-prefixed game frame (`gsb_net::ws`).
    Ws,
}

impl std::fmt::Display for ListenerTransport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            Self::Tcp => "tcp",
            Self::Tls => "tls",
            Self::Udp => "udp",
            Self::Quic => "quic",
            Self::Ws => "ws",
        };
        f.write_str(s)
    }
}

/// One `[[listeners]]` entry: an independent socket that accepts clients
/// into the SAME rooms as every other listener (rooms/actors are
/// transport-agnostic by design; only the composition root ever picks a
/// transport).
///
/// Per-entry keys are deliberately minimal: `transport` + `bind` are the
/// identity of a listener; `tls_cert`/`tls_key` exist because each TLS
/// listener legitimately owns its own certificate (e.g. an internal-CA
/// listener next to a public-CA one on different addresses). Everything
/// else stays a GLOBAL knob on purpose (simplest sound choice): frame
/// limits, channel capacities, idle windows and the rUDP budget/key are
/// deployment-wide policies of ONE actor stack, not properties of a
/// socket — per-listener overrides would fork the pipeline's semantics
/// per door for no demonstrated need.
///
/// Any other key in an entry refuses to parse (BACKLOG F61), naming the
/// key, the keys an entry takes, and the line it sits on: a typo, or a
/// server-wide key written inside a door (`listen_backlog`), used to be
/// silently dropped. The entry is flat — no per-transport sub-table, no
/// flattened or tagged part — so serde's own `deny_unknown_fields`
/// covers it whole, with the message `[rooms.<id>]` and `[metrics]` give.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ListenerEntry {
    /// Which transport this listener serves (`"tcp"`, `"tls"`, `"udp"`,
    /// `"quic"`, `"ws"`).
    pub transport: ListenerTransport,
    /// Socket address to bind (e.g. `"0.0.0.0:7777"`, `"127.0.0.1:0"`).
    /// Required: an unnamed door is a config mistake, not a default.
    pub bind: String,
    /// Path to the PEM certificate chain (leaf first) — REQUIRED (with
    /// `tls_key`) when `transport = "tls"` or `transport = "quic"` (QUIC
    /// is TLS 1.3 underneath; both doors load the same identity), and
    /// forbidden otherwise (a "tcp"/"udp"/"ws" entry carrying TLS files
    /// is a startup error, never a silent reinterpretation of the entry).
    pub tls_cert: Option<String>,
    /// Path to the PEM private key matching [`Self::tls_cert`] — see there.
    pub tls_key: Option<String>,
}

/// Server configuration (see `config.example.toml`).
///
/// Its top level is shared with the hosted game, which reads its own
/// part of the file from [`Self::raw`] (its `[<game>]` table, or flat
/// keys) — and a file may carry several games' tables. So the struct
/// itself takes any key; the server then refuses, before anything binds,
/// a top-level key that is neither one of these fields nor owned by a
/// game compiled into the build (BACKLOG F62,
/// [`Self::check_top_level_keys`], `GameModule::owned_keys`). Every
/// table the engine owns below it refuses an unknown key when it parses:
/// a `[[listeners]]` entry, `[rooms.<id>]`, `[metrics]`,
/// `[metrics.otlp]`.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(default)]
pub struct Config {
    /// Socket address to bind (e.g. `"0.0.0.0:7777"`, `"127.0.0.1:0"`).
    pub bind: String,
    /// Global tick rate in ticks per second. Every room must run at a rate
    /// that divides this one (a room at `global / k` steps every k-th tick).
    pub tick_hz: f64,
    /// Number of rooms to pre-create at startup (ids `1..=room_count`).
    pub room_count: u64,
    /// Maximum frame body size in bytes (transport-level guard).
    pub max_frame_bytes: usize,
    /// Capacity of each room's control channel (join/leave/shutdown).
    pub room_control: usize,
    /// Capacity of each connection's action channel (inputs buffered until
    /// the room's next tick).
    pub conn_action: usize,
    /// Capacity of each connection's inbound (frames in) mailbox.
    pub conn_inbox: usize,
    /// Capacity of each connection's outbound (batches out) channel.
    pub conn_out: usize,
    /// Session-lifecycle idle window, in seconds: a connection that sends
    /// *no* inbound frame (heartbeat, input, anything) for this long is
    /// closed by the server on its own initiative (a gentle `ERROR` frame,
    /// code 9, then EOF). This is the half-open-TCP guardrail — a client
    /// whose cable was pulled or power lost sends no FIN/RST, and its
    /// 3 tasks + 2 channels + registry entry would otherwise sit until
    /// process death. `0` disables the check.
    ///
    /// Default 30 s: comfortably above the ~10 s heartbeat cadence a
    /// well-behaved client should keep (any inbound frame resets the
    /// window, so a live client can never trip it), and short enough that
    /// a dead-but-open connection is detected within a minute. Clients
    /// that never send *anything* (not even heartbeats) must stay below
    /// this with whatever traffic they do send.
    pub idle_timeout_secs: f64,
    /// Session-lifecycle write-stall window, in seconds: a connection
    /// whose socket has accepted not one BYTE for this long — while the
    /// server had something to write — is closed by the server
    /// on its own initiative, through the ordinary teardown. This is the
    /// other half of the socket from [`Self::idle_timeout_secs`], and the
    /// two are a pair: inbound silence cannot see a peer that keeps its
    /// connection open and simply stops READING. Such a peer's receive
    /// window closes, the writer pump parks inside its socket write, the
    /// outbound channel stays FULL (never closed, so the half-dead
    /// teardown never fires), the room drops a frame every tick, and the
    /// session holds its room slot and registry row while receiving
    /// nothing. `0` disables the check.
    ///
    /// Default 10 s. The bound is on PROGRESS, not age — and on BYTES, not
    /// frames — so it does not touch the "a slow client is tolerated"
    /// contract: every byte the socket accepts restarts the window, even
    /// in the middle of a frame that takes longer than the window to
    /// drain, so a client that is merely BEHIND keeps its session while
    /// its dropped snapshots are counted (`dropped_frames`) exactly as
    /// before. Ten seconds of a socket accepting not one byte is not
    /// slowness; on a loopback or LAN path it is hundreds of kilobytes of
    /// kernel buffer that stopped moving entirely. (One caveat from the
    /// kernel, not the clock: Linux wakes a writer blocked on a full send
    /// buffer only once roughly a third of it has drained, so with a send
    /// buffer of B bytes a reader slower than about B / (3 × window) is
    /// still seen as silent between wake-ups.)
    pub write_stall_secs: f64,
    /// Per-room membership cap (see `RoomConfig::max_players`); a join
    /// into a full room is rejected with `ERROR` code 8 (the connection
    /// stays alive). `None` = unlimited.
    pub max_players: Option<u32>,
    /// Server-wide connection cap, enforced at connection birth by the
    /// registry (the count lives in its table, so the guardrail does —
    /// the accept loop cannot see disconnects without a second awaited
    /// source). A rejected connection gets `ERROR` code 9 + EOF and no
    /// registry entry. `None` = unlimited.
    ///
    /// Default 100_000: the design goal itself (DESIGN §1, "100k+ concurrent
    /// connections") as a hard guardrail — beyond the goal is an unmeasured
    /// region, and the cap keeps the server's behavior there defined (gentle
    /// rejection) instead of unbounded resource growth.
    pub max_connections: Option<u64>,
    /// Cap on simultaneously UNAUTHENTICATED connections
    /// (docs/SECURITY.md §4): a scripted handshake storm must not grow
    /// server memory without bound while the total cap is still far away.
    /// A connection over this cap is rejected at birth (`ERROR` code 9,
    /// "server at unauthenticated capacity", no registry entry) — exactly
    /// the [`Self::max_connections`] rejection path, checked in addition
    /// to it. Authenticated connections (and detached/resumed sessions —
    /// they carry tickets, so they are authenticated by construction)
    /// never count against it.
    ///
    /// Value semantics (resolved ONCE at startup, see `unauth_cap_of`):
    ///
    /// - omitted (`None`, the default): derived as
    ///   `max(max_connections / 4, 64)` — 25 % of the total cap, floored
    ///   at 64 so even a tiny deployment keeps real headroom for a lobby
    ///   full of slow-but-honest handshakes;
    /// - when `max_connections` is unlimited: the same formula runs
    ///   against the built-in default base (`DEFAULT_MAX_CONNECTIONS`,
    ///   so the derived default is 25_000). Decision (the contract left
    ///   this open, "simplest sound choice wins"): an unlimited-total
    ///   server still needs a bounded half-open-handshake pool, and taking
    ///   the formula's base from the documented design-goal constant keeps
    ///   ONE derivation instead of two behaviors — while 25 k simultaneous
    ///   pre-auth handshakes is far beyond any legitimate slow-auth flow
    ///   yet still a hard bound on storm memory;
    /// - `n > 0`: used exactly;
    /// - `0`: the cap is DISABLED (the config-file convention here: 0 =
    ///   unlimited) for deployments behind an external gate.
    pub max_unauth_conns: Option<u64>,
    /// Warn when a room group's snapshot payload exceeds this many bytes
    /// (on rUDP such a payload is fragmented — a bandwidth/fragmentation
    /// signal; default = `max_frame_bytes`).
    pub max_snapshot_bytes: usize,
    /// Keep-alive rate for unchanged snapshot groups, in Hz (a client that
    /// lost its last snapshot must not stay stale forever). `<= 0` disables.
    pub keepalive_hz: f64,
    /// **Input-idle ceiling**, in seconds — omitted (`None`, the default)
    /// = OFF, and `0` = OFF too.
    ///
    /// A capacity safety valve, not an AFK policy: AFK is the game's call
    /// (the base publishes the SIGNAL unconditionally — see
    /// `gsb_core::room::IdleView` — and a game writes its rule on it).
    /// When set, a member whose last ACTION-bearing frame is at least
    /// this old is handed to the room's ordinary disconnect policy
    /// (`on_disconnect`), which decides park / AI handover / despawn.
    /// Heartbeats keep a session alive and never reset this clock.
    pub max_idle_input_secs: Option<u64>,
    /// **What the input-idle ceiling does** (BACKLOG E6) — `"leave_room"`
    /// or `"disconnect"`; omitted (`None`, the default) = the game's
    /// default (`GameModule::afk_action`), `"leave_room"` when the game
    /// has none. Every hosted room's `RoomConfig::afk_action`.
    ///
    /// Both first hand the member to `on_disconnect` (park / AI handover
    /// / despawn — the game's call). `"leave_room"` stops there: the
    /// socket stays open and the client may join again (today's
    /// behaviour). `"disconnect"` also closes the connection: the client
    /// gets a best-effort ERROR 9 (`input idle: …`) and the close,
    /// counted as `server_closes{reason="idle_input"}`; a parked entity
    /// stays resumable. No effect without
    /// [`Self::max_idle_input_secs`].
    #[serde(deserialize_with = "afk_action::deserialize")]
    pub afk_action: Option<gsb_core::room::AfkAction>,
    /// **Detach-hold ceiling** — every hosted room's
    /// `RoomConfig::max_detach_hold` (`docs/RECONNECT.md` §17): the
    /// longest a game's `may_release` veto can keep a disconnected
    /// player's entity, measured from the disconnect. It only overrides a
    /// VETO; it never shortens a grace.
    ///
    /// Spelled `max_detach_hold_secs` in the file: seconds (`>= 0`,
    /// fractions allowed), or `"off"` for no ceiling (`None` — a veto
    /// holds while it stands; only for a trusted game). `0` is literal:
    /// no extension — a veto is overridden the first time it is asked
    /// (unlike [`Self::max_idle_input_secs`]'s "0 = off": zero has a safe
    /// meaning here, and "off" by accident would be the unbounded lock).
    /// Omitted: the core's default, 10 min
    /// (`gsb_core::room::DEFAULT_MAX_DETACH_HOLD`).
    #[serde(
        rename = "max_detach_hold_secs",
        deserialize_with = "detach_hold::deserialize"
    )]
    pub max_detach_hold: Option<std::time::Duration>,
    /// **Per-connection input rate limit** (BACKLOG E1; docs/SECURITY.md
    /// "post-auth input volume"), in actions a second — omitted (`None`,
    /// the default) = the game's default (`GameModule::input_rate`), off
    /// when the game has none; `0` = OFF even over a game's default.
    ///
    /// Every hosted room's `RoomConfig::input_rate`: a token bucket per
    /// connection over its valid game-band input. Input over it is
    /// dropped by the connection actor before the room sees it, counted
    /// (`input_rate_limited`), and never scored as a violation; control
    /// frames and RPC requests are not metered. The number is a gameplay
    /// parameter — set it from the game's real input cadence with
    /// headroom, never below what an honest client sends.
    pub input_rate_hz: Option<u32>,
    /// The bucket of [`Self::input_rate_hz`]: the most actions admitted
    /// at once (a burst after a lag spike). Omitted = one second's worth
    /// (`input_rate_hz`). Only with `input_rate_hz > 0` in the same table:
    /// alone, next to `input_rate_hz = 0`, or `0` refuses startup.
    pub input_burst: Option<u32>,
    /// **Per-room overrides** (`[rooms.<id>]`, BACKLOG B18): one room's
    /// own values for the room-level keys above (`tick_hz`,
    /// `room_control`, `conn_action`, `max_snapshot_bytes`,
    /// `keepalive_hz`, `max_players`, `max_idle_input_secs`,
    /// `afk_action`, `max_detach_hold_secs`, `input_rate_hz`,
    /// `input_burst` — same
    /// spellings, same meanings), laid over
    /// the server's room for that id alone: a boot room of that id, an
    /// admin `POST /rooms/open?id=` of it (the query's `tick_hz` on top),
    /// and [`Config::room_config`]. Empty (the default) = every room is
    /// the server's room, as before.
    ///
    /// Refused at startup: a key that is not room-level (server-wide
    /// keys stay global; a game's settings live in its own table), an
    /// id that is not a positive integer written plainly, and a room the
    /// registry would refuse — a `tick_hz` that does not divide the
    /// global rate, a `keepalive_hz` above the room's `tick_hz`. An id
    /// past `room_count` is valid: it is the room an admin open of that
    /// id builds.
    #[serde(deserialize_with = "room::deserialize_overrides")]
    pub rooms: std::collections::BTreeMap<u64, RoomOverride>,
    /// The TOPOLOGY selection axis (`"single"` | `"sharded"`; see
    /// [`Topology`]): who computes the world and as how many authoritative
    /// pieces.
    ///
    /// Value semantics (resolved ONCE at startup, see
    /// `Config::resolve_selection`):
    ///
    /// - omitted (`None`, the default): DERIVED — `single`, except when the
    ///   legacy `visibility` key reads `"sharded"` (that spelling was
    ///   always a topology statement wearing a visibility name);
    /// - set: the explicit key WINS over the legacy derivation. When the
    ///   two disagree (e.g. legacy `visibility = "sharded"` next to
    ///   explicit `topology = "single"`), a startup warn names which side
    ///   took effect (the same courtesy `[[listeners]]` extends to the
    ///   scalar transport keys) — behavior never changes silently.
    pub topology: Option<Topology>,
    /// The COMMUNICATION selection axis (`"always-full"` | `"delta"`;
    /// see [`Communication`]): how snapshot data is packaged for clients.
    ///
    /// Value semantics (resolved ONCE at startup, see
    /// `Config::resolve_selection`):
    ///
    /// - omitted (`None`, the default): DERIVED from the visibility axis —
    ///   `spatial` ⇒ `delta` (its room diffs per cell internally), every
    ///   other visibility ⇒ `always-full`. A derived value DESCRIBES what
    ///   the mapped room already does; it requests nothing new;
    /// - set: the explicit key WINS over the derivation, and an explicit
    ///   `"delta"` is a REQUEST for client-facing delta snapshots — served
    ///   only where a delta implementation exists today (`single ×
    ///   spatial`, the same AoiRoom the derived spelling builds); every
    ///   other combination REFUSES STARTUP with an error naming the
    ///   roadmap phase that will deliver them (never a silent downgrade
    ///   to full frames).
    pub communication: Option<Communication>,
    /// The legacy input encoding of TWO of the three selection axes (see
    /// [`Visibility`] and the derivation below).
    ///
    /// The four non-`"sharded"` spellings ARE the `VisibilityAxis`
    /// values. `"sharded"` decodes to topology = `sharded` + visibility =
    /// `all`. Full derivation table (when the new keys are omitted):
    ///
    /// | legacy `visibility` | resolved triple (topology × visibility × communication) |
    /// |---|---|
    /// | `"all"`     | single  × all     × always-full |
    /// | `"spatial"` | single  × spatial × delta¹ |
    /// | `"team"`    | single  × team    × always-full |
    /// | `"pvs"`     | single  × pvs     × always-full |
    /// | `"sharded"` | sharded × all     × always-full |
    ///
    /// ¹ names the packaging the spatial rooms ALREADY serve (the
    /// single-world per-cell diff; on the grid, the Faz B composite via
    /// an explicit `topology = "sharded"`): the explicit
    /// [`Self::communication`] key resolves to the same room there;
    /// elsewhere an explicit `"delta"` is rejected until its codec
    /// ships.
    ///
    /// Precedence: an explicit [`Self::topology`] /
    /// [`Self::communication`] key always overrides its derived cell.
    /// Every legacy spelling resolves to one of the five supported
    /// combinations, so pre-axes configs keep working unchanged; only
    /// EXPLICIT new-axis requests can reach a rejected combination, and
    /// those fail at startup naming the roadmap phase that will deliver
    /// them (see [`ServerError`]).
    pub visibility: Visibility,
    /// Number of shards per room (used only when the RESOLVED topology is
    /// [`Topology::Sharded`] — via legacy `visibility = "sharded"` OR an
    /// explicit `topology = "sharded"`). The map is divided
    /// into a near-square grid of `rows × cols` shards (`rows * cols =
    /// shard_count`, see [`gsb_demo::sharded::grid_shape`]). Must be
    /// 1..=256 (the grid topology); validated at startup. Default 4 (2×2).
    pub shard_count: u32,
    /// The transport (see [`TransportKind`]).
    pub transport: TransportKind,
    /// The rUDP datagram budget in bytes (transport-level guard; default
    /// 1472 = MTU 1500 − IP 20 − UDP 8). Used only when
    /// [`Self::transport`] = `Udp`. See `gsb_net::udp` (feature 3: a
    /// game-band frame over it is fragmented, up to 16 fragments; past
    /// that, and for control frames, it cannot be sent).
    pub udp_max_datagram_bytes: usize,
    /// The rUDP cookie key as 32 hex characters (16 bytes). Used only
    /// when [`Self::transport`] = `Udp`. `None` (the default) = draw the
    /// key from the OS entropy source at bind time; if that draw fails
    /// the server refuses to start — a predictable key would invert the
    /// handshake's anti-amplification property (see `gsb_net::udp`).
    pub udp_cookie_key: Option<String>,
    /// The rUDP writers' **congestion response** (rUDP hardening round
    /// 3): `"off"` (the default) — every writer sends at once, the door
    /// as it was; `"pace"` — a session whose client reports (the
    /// default `UdpClient`) and whose path cannot carry what the room
    /// sends is paced to the path's estimated rate, and the oldest
    /// game-band frames it cannot send within 50 ms are dropped and
    /// counted (`udp_game_frames_dropped_paced`); the control band is
    /// never paced or dropped, and a client that does not report is
    /// never paced. Every rUDP door, whichever grammar declared it; not
    /// a `[[listeners]]` key. See `gsb_net::udp` ("congestion").
    pub udp_congestion: UdpCongestionKind,
    /// rUDP **connection migration** (BACKLOG B3): on, a session survives
    /// the client's address change — a NAT rebinding, a Wi-Fi ↔ cellular
    /// handover — after path validation: no new handshake, no resume.
    /// Off, an address change is a new session and a resume. Unset (the
    /// default) = **on for a sealed door** (B5a/B112, decision 5: only an
    /// authenticated, newest record starts a validation and the challenge
    /// is sealed) and **off for a plaintext door** (there the connection
    /// id is a bearer token: an on-path sniffer that reads it can steer
    /// the session's server → client stream to itself —
    /// `docs/RUDP-SECURITY.md` §3, §7; a plaintext door with it off is
    /// byte for byte what it was). Every rUDP door; not a `[[listeners]]`
    /// key. See `gsb_net::udp` (modules `path`, `sealed`).
    pub udp_migration: Option<bool>,
    /// rUDP **record layer** (BACKLOG B5a, `docs/RUDP-SECURITY.md`):
    /// `"sealed"` (the default — production) runs Noise
    /// `NK_25519_ChaChaPoly_BLAKE2s` inside the cookie handshake (0 extra
    /// round trips) and seals every session datagram both ways under the
    /// server's static key, which clients pin (the platform hands its
    /// public half out with the ticket); a sealed door REQUIRES
    /// [`Self::udp_static_key`] or [`Self::udp_static_key_file`] — without
    /// one the server refuses to start (never a silent plaintext
    /// fallback). `"plaintext"`: the door before B5a, for dev/LAN only
    /// (one startup warning). A plaintext client at a sealed door is
    /// refused at the handshake (`udp_proofs_refused_plaintext`), a
    /// sealed client at a plaintext door refuses it — the one deliberate
    /// compatibility break of the rUDP line (`docs/DESIGN.md` §5). Every
    /// rUDP door; not a `[[listeners]]` key.
    pub udp_security: UdpSecurityKind,
    /// The sealed rUDP doors' **static X25519 private key** as 64 hex
    /// characters (32 bytes; any 32 random bytes are a valid key —
    /// `openssl rand -hex 32`). The server logs only the PUBLIC half (at
    /// bind, `public_key=`), which is what clients pin. Prefer
    /// [`Self::udp_static_key_file`] in production (the key stays out of
    /// the config file); setting both refuses startup.
    pub udp_static_key: Option<String>,
    /// A file holding the static key ([`Self::udp_static_key`]'s 64 hex
    /// characters; surrounding whitespace ignored). Read at startup; a
    /// missing, unreadable or malformed file refuses startup (the error
    /// names the file, never its content).
    pub udp_static_key_file: Option<String>,
    /// The sealed rUDP doors' **handshake budget** (BACKLOG B119): Noise
    /// handshakes (each ~180 µs of one core of the door's one demux task,
    /// B110) the door starts per second — a token bucket after the
    /// cookie and the per-source cap, before the Diffie-Hellman, holding
    /// 50 ms of the rate. A verified proof over it creates nothing and is
    /// counted (`udp_proofs_refused_budget`); the client re-sends it
    /// (≤ 200 ms later). Default 1000/s (~18 % of a demux core, a
    /// 1000-player join storm in about a second); `0` = no budget. Per
    /// door; every rUDP door gets the same value.
    pub udp_handshakes_per_sec: Option<u32>,
    /// Path to the server certificate chain, PEM (leaf first). Empty (the
    /// default) = plaintext TCP, byte-identical behavior to before the TLS
    /// turn. Set together with [`Self::tls_key`] it serves TCP over rustls
    /// (docs/SECURITY.md §2). Setting one WITHOUT the other is a startup
    /// error — no silent half-configured fallback; setting either with
    /// `transport = "udp"` is also a startup error (rUDP is experimental
    /// and takes no TLS).
    pub tls_cert: String,
    /// Path to the PEM private key matching [`Self::tls_cert`]. See there.
    pub tls_key: String,
    /// The listener table (`[[listeners]]`): MULTIPLE independent sockets
    /// serving the ONE room/map simultaneously — e.g. a TLS-TCP door for
    /// paying clients next to a plain-TCP door for a LAN build, an rUDP
    /// or QUIC door beside both, and a WebSocket door for browser-adjacent
    /// clients. Every accepted endpoint flows into the SAME
    /// pipeline (same registry, same rooms, one shared connection-id
    /// sequence), so which door a client walked in through is invisible
    /// above the accept loop.
    ///
    /// Semantics:
    ///
    /// - ABSENT (the default): exactly ONE listener is DERIVED from the
    ///   legacy scalar keys (`transport` + `bind` + `tls_cert`/`tls_key`),
    ///   byte-identical to pre-multi-listener behavior. Existing configs,
    ///   tests and deployments are untouched.
    /// - PRESENT and non-empty: the array WINS; the legacy scalar keys are
    ///   ignored. A startup warn fires when any legacy scalar differs from
    ///   its built-in default, so an operator who set both sees which one
    ///   took effect (the config parser cannot distinguish "explicitly set
    ///   to the default value" from "omitted", so identical-to-default
    ///   legacy keys stay silent).
    /// - PRESENT but empty: a startup error — a server with zero doors is
    ///   never a valid deployment, and silently falling back to the scalar
    ///   keys would hide the mistake.
    pub listeners: Option<Vec<ListenerEntry>>,
    /// **Accept backlog** of every TCP-based listening socket (BACKLOG
    /// B84): the plain TCP, TLS and WebSocket doors — whichever grammar
    /// declared them — and the ops HTTP surface ([`Self::http_listen`]).
    /// The queue of connections the kernel has completed and the server
    /// has not accepted yet; a join storm past it loses SYNs (counted as
    /// `ListenOverflows` on Linux) and those clients retry a second
    /// later. UDP doors (rUDP, QUIC) have no accept queue and ignore it.
    ///
    /// Default 128 (`gsb_net::listen::DEFAULT_LISTEN_BACKLOG`): what
    /// tokio's own bind passes, i.e. the queue every door had before the
    /// key existed. The kernel caps it: the queue a socket gets is
    /// `min(listen_backlog, somaxconn)` (Linux `net.core.somaxconn`,
    /// 4096 by default since 5.4) — a larger value is not an error, it
    /// is capped. `1..=2147483647` (a C `int`); `0` or more refuses
    /// startup ([`ServerError::BadListenBacklog`]). One knob for every
    /// door: like the other socket-wide knobs it is not a
    /// `[[listeners]]` key.
    pub listen_backlog: u32,
    /// **Per-source handshake cap** of every handshaking door (BACKLOG
    /// D11): the WebSocket, TLS and QUIC doors, whichever grammar
    /// declared them. One source address — an IPv4 address, an IPv6 /64
    /// — holds at most this many of a door's handshake slots (the door's
    /// own bound is the pre-auth cap, `max_unauth_conns`); a connection
    /// over it is closed unhandshaken (QUIC: refused, or asked to prove
    /// an unproven address with a Retry) and counted
    /// (`handshakes_refused_per_source`, `handshakes_retried_per_source`).
    /// Plain TCP has no handshake stage and ignores it. On an rUDP door
    /// the cookie exchange holds nothing, so the cap is on the state it
    /// creates (BACKLOG B89): one source's sessions established by a
    /// verified proof and not yet taken by the accept loop; a proof over
    /// it creates nothing, gets no accept and is counted
    /// (`udp_proofs_refused_per_source`) — the client re-sends it.
    ///
    /// `None` (the default) or `0` = no per-source cap: the doors as they
    /// were. Off by default because the right number is the deployment's:
    /// players behind one NAT address (a LAN party, a carrier-grade NAT)
    /// share it, and every client of a test or load run connects from one
    /// loopback address. It counts handshakes IN FLIGHT (an honest one
    /// lasts a round trip or two), not connections — size it to the
    /// players behind one address who may connect within the same
    /// second, with headroom (docs/SECURITY.md §4.3).
    pub max_handshakes_per_source: Option<u32>,
    /// **Per-source cap on unauthenticated connections** (BACKLOG D12):
    /// one source address — an IPv4 address, an IPv6 /64, the
    /// [`Self::max_handshakes_per_source`] rule — holds at most this many
    /// of the server's unauthenticated connections (the pool
    /// [`Self::max_unauth_conns`] caps), whichever door they came
    /// through. Plain TCP has no handshake stage: its peers are
    /// unauthenticated from the first byte, and without this one address
    /// could fill the whole pool. The other doors' sessions join the pool
    /// once their handshake ends, so they count too — after the
    /// handshake, never twice at once (the handshake slot is given back
    /// before the session is registered). A connection over it is refused
    /// at birth like the pool's own refusals (`ERROR` code 9, no registry
    /// entry; WebSocket close 1013) and counted
    /// (`server_closes{reason="unauth_source_cap"}`). A session leaves the
    /// count when it authenticates or closes; a failed AUTH keeps it
    /// (the session is still unauthenticated).
    ///
    /// `None` (the default) or `0` = no per-source cap. Off by default for
    /// [`Self::max_handshakes_per_source`]'s reasons (NAT, one loopback
    /// address in tests and load runs). A sibling of that key, not the
    /// same one: it counts sessions that may wait a whole AUTH round trip
    /// (a ticket validator's), not handshakes in flight, on every door.
    pub max_unauth_conns_per_source: Option<u32>,
    /// **Receive buffer** (`SO_RCVBUF`, bytes) of every UDP-based door's
    /// socket (BACKLOG B4): the rUDP doors and the QUIC doors, whichever
    /// grammar declared them. ONE socket carries every session of a UDP
    /// door, so its kernel receive queue is the only buffer under a
    /// burst (a join storm); a datagram arriving while it is full is
    /// dropped by the kernel before the server sees it (Linux
    /// `Udp: RcvbufErrors`). The TCP-based doors ignore it.
    ///
    /// `None` (the default) = not touched: no `setsockopt`, the system
    /// default (Linux `net.core.rmem_default`) — the socket every UDP
    /// door had before the key existed. Linux caps the value at
    /// `net.core.rmem_max` and doubles it (the second half is the
    /// kernel's bookkeeping); a capped value is a startup warning, not an
    /// error. `4096..=2147483647` (a page to a C `int`); anything else
    /// refuses startup ([`ServerError::BadUdpBuffer`]). One knob for
    /// every UDP door, like `listen_backlog` for the TCP ones.
    pub udp_recv_buffer_bytes: Option<u32>,
    /// **Send buffer** (`SO_SNDBUF`, bytes) of every UDP-based door's
    /// socket — the same rules as [`Self::udp_recv_buffer_bytes`] on the
    /// way out (Linux `net.core.wmem_max`; a full one fails or parks a
    /// datagram send, counted by band on rUDP).
    pub udp_send_buffer_bytes: Option<u32>,
    /// World units per AOI cell edge (used when the resolved visibility
    /// axis is `Spatial` — the single-world AoiRoom AND the sharded ×
    /// spatial composite's per-shard cells). See `gsb_demo::aoi` for the
    /// `max_snapshot_bytes` / density relation and the measured
    /// break-even.
    pub aoi_cell_size: f32,
    /// World units an enemy must be within to be visible to a team (used
    /// only when [`Self::visibility`] = `Team`). See `gsb_demo::team` for
    /// the vision source model.
    pub team_vision_radius: f32,
    /// Half-size of the square map entities spawn on (all four demo rooms;
    /// default 50 = the historical 100×100 arena, bit-identical). A load
    /// profile that places entities on a "wide map" (the generator's
    /// `spread` profile) pairs a large value here with the same value in
    /// the clients' `--spawn-half-size`, so spawn points and targets live
    /// on the same map and the run is statistically steady from tick 1.
    /// The PVS strategy's *visibility map* stays its hand-authored 100×100
    /// sectors regardless (see `gsb_demo::pvs::SectorRoom::spawn_half`).
    pub spawn_half_size: f32,
    /// The demo rooms' disconnect-park grace, in seconds (`config.example.toml`:
    /// `disconnect_grace_secs`; RECONNECT §3): how long a dropped
    /// transport's entity STAYS in the world — visible in snapshots,
    /// holding its room-cap slot — before the hold ends toward the bot
    /// handover ([`ExpireTo::AiHandover`](gsb_core::room::ExpireTo::AiHandover); the demo stub bot then keeps
    /// playing the hero through the ordinary input path). A human who
    /// rejoins inside the window resumes onto the live entity with the
    /// same wire id (the implicit resume, §14.3).
    ///
    /// Default 30 s; `0` restores the pre-reconnect semantics exactly
    /// (disconnect = despawn). Flows into every room the factories build
    /// (the game-level knob rides the factory closure like
    /// `spawn_half_size`, not [`RoomConfig`](gsb_core::room::RoomConfig) — it is policy, not core
    /// mechanics).
    pub disconnect_grace_secs: f64,
    /// Bind address of the HTTP ops surface (`docs/OPS.md`): `/healthz`,
    /// `/metrics`, `/rooms`, and the room-admin writes. The empty string
    /// (the default) DISABLES the listener entirely — the default
    /// deployment gains no extra socket and no new attack surface. When
    /// set, it also redirects the metrics reports into the surface's
    /// `watch` snapshot (see `start_inner`). WHY localhost by convention:
    /// v1 ships no auth/TLS (OPS §5), so anything that can reach this port
    /// can scrape metrics AND open/close rooms — bind beyond loopback only
    /// as an explicit, network-guarded decision (e.g. `"127.0.0.1:9090"`,
    /// never `"0.0.0.0"`).
    pub http_listen: String,
    /// **Concurrent connection cap** of the ops HTTP surface (BACKLOG
    /// B49): over it a new connection is closed at once, unread and
    /// unanswered, and counted (`ops_http_conns_refused`). Default 64:
    /// the surface's callers are a scraper or two, health probes and an
    /// operator's `curl`, each connection living at most the head
    /// deadline (5 s) + [`Self::http_write_timeout_secs`] + a 300 ms
    /// drain — an order of magnitude of headroom, and a bound on the
    /// tasks (and response buffers) one peer can pin. `0` (like the
    /// other caps) or `None` = no cap.
    pub http_max_connections: Option<u32>,
    /// **Response write deadline** of the ops HTTP surface, in seconds
    /// (BACKLOG B49): the whole response write — one timeout around it,
    /// not per write — of a peer that sent its request and does not read
    /// the answer; past it the connection is closed and counted
    /// (`ops_http_writes_timed_out`). Default 10 s, the game doors' write
    /// stall: a multi-megabyte `/metrics` crosses a loopback or LAN in
    /// milliseconds, and even a slow admin link in seconds. `0` (or a
    /// negative or non-finite value) disables it, as `write_stall_secs`.
    pub http_write_timeout_secs: f64,
    /// **Routing deadline** of the ops HTTP surface, in seconds (BACKLOG
    /// B90): the whole routing step of one request — the room
    /// bookkeeper's and the registry's answers that `/rooms` and the room
    /// open/close wait for, queueing on a full registry mailbox included;
    /// one timeout around it, like the head read and the write. Past it
    /// the request is answered `504 Gateway Timeout` (the outcome of an
    /// open or close is unknown: the registry may still apply it — both
    /// are idempotent, a retry is safe) and counted
    /// (`ops_http_routes_timed_out`). Default 10 s: a healthy registry
    /// answers in microseconds, and even a full join-storm mailbox (4096
    /// messages) should drain in under a second (an estimate, not
    /// measured), so only a stalled registry reaches it,
    /// while the connection task stays bounded by the same order as its
    /// other two deadlines. `0` (or a negative or non-finite value)
    /// disables it, as `http_write_timeout_secs`.
    pub http_route_timeout_secs: f64,
    /// The export layer's push exporters (`[metrics]`, docs/OPS.md §6):
    /// `[metrics.otlp]` pushes every report interval to an OpenTelemetry
    /// collector. Empty (the default) = no push; the log lines and the
    /// ops surface's `/metrics` are unaffected either way. A table the
    /// build has no exporter for refuses startup
    /// ([`ServerError::OtlpNotBuilt`]).
    pub metrics: MetricsConfig,
    /// The game this server hosts (docs/GAME-MODULE.md §6 decision 3): the
    /// name of one of the games compiled into the build (cargo features;
    /// see `gsb_server::games::compiled_in`). Default `"demo"`, the 2D
    /// demo every pre-module config runs. An unknown name refuses startup
    /// with the list of the compiled-in games.
    pub game: String,
    /// The parsed config file, kept whole for the game module
    /// (GAME-MODULE §4.3): a module reads its own keys from it and tells
    /// an EXPLICITLY written key from a defaulted one — which the typed
    /// fields above cannot, as every field has a default. Filled by
    /// [`Config::from_file`]; empty for a config built in code (nothing
    /// was written explicitly).
    #[serde(skip)]
    pub raw: toml::Table,
    /// Where the file wrote each top-level key of [`Self::raw`] (BACKLOG
    /// F64): a refused top-level key names its `path:line`. Filled by
    /// [`Config::from_file`]; empty for a config built in code (its
    /// refusals name no place). Opaque — only the loader fills it.
    #[serde(skip)]
    pub origin: crate::config::ConfigOrigin,
}

/// The built-in default server-wide connection cap (DESIGN §1's design
/// goal as a guardrail). Single source of truth for the `Config` default
/// AND the unauth-cap derivation when `max_connections` is unlimited.
pub(crate) const DEFAULT_MAX_CONNECTIONS: u64 = 100_000;

/// The ops HTTP surface's default cap on live connections (B49; see
/// `Config::http_max_connections`).
pub(crate) const DEFAULT_HTTP_MAX_CONNECTIONS: u32 = 64;

/// The ops HTTP surface's default response write deadline, in seconds
/// (B49; see `Config::http_write_timeout_secs`).
pub(crate) const DEFAULT_HTTP_WRITE_TIMEOUT_SECS: f64 = 10.0;

/// The ops HTTP surface's default routing deadline, in seconds (B90; see
/// `Config::http_route_timeout_secs`).
pub(crate) const DEFAULT_HTTP_ROUTE_TIMEOUT_SECS: f64 = 10.0;

/// The floor of the derived unauthenticated-connection cap
/// (`unauth_cap_of`): even the smallest deployment gets real headroom for
/// slow-but-honest handshakes instead of a cap that rounds to near-zero.
pub(crate) const MIN_UNAUTH_CONNS: u64 = 64;

// The demo's defaults for its flat keys (GAME-MODULE §6 decision 1),
// spelled out so `Config` builds with no game compiled in. The demo
// module's tests lock each one to `gsb-demo`'s own constant.

/// `team_vision_radius`'s default (`gsb_demo::team::DEFAULT_VISION_RADIUS`).
pub(crate) const DEMO_DEFAULT_VISION_RADIUS: f32 = 25.0;
/// `spawn_half_size`'s default (`gsb_demo::room::DEFAULT_SPAWN_HALF`).
pub(crate) const DEMO_DEFAULT_SPAWN_HALF: f32 = 50.0;
/// `disconnect_grace_secs`'s default (`gsb_demo::DEFAULT_DISCONNECT_GRACE`).
pub(crate) const DEMO_DEFAULT_DISCONNECT_GRACE_SECS: f64 = 30.0;

impl Default for Config {
    fn default() -> Self {
        Self {
            bind: "0.0.0.0:7777".into(),
            tick_hz: 30.0,
            room_count: 1,
            max_frame_bytes: gsb_net::tcp::DEFAULT_MAX_FRAME_BYTES,
            room_control: 128,
            conn_action: 256,
            conn_inbox: 1024,
            conn_out: 256,
            idle_timeout_secs: 30.0,
            write_stall_secs: 10.0,
            max_players: Some(10_000),
            max_connections: Some(DEFAULT_MAX_CONNECTIONS),
            max_unauth_conns: None,
            max_snapshot_bytes: gsb_net::tcp::DEFAULT_MAX_FRAME_BYTES,
            keepalive_hz: 1.0,
            // OFF: AFK is a game decision, so the base's ceiling stays
            // invisible until an operator asks for it.
            max_idle_input_secs: None,
            // The game's (or `leave_room`): the ceiling keeps the socket.
            afk_action: None,
            max_detach_hold: Some(gsb_core::room::DEFAULT_MAX_DETACH_HOLD),
            // OFF: the number is the game's (or the operator's).
            input_rate_hz: None,
            input_burst: None,
            rooms: std::collections::BTreeMap::new(),
            topology: None,
            communication: None,
            visibility: Visibility::default(),
            shard_count: 4,
            transport: TransportKind::default(),
            udp_max_datagram_bytes: gsb_net::udp::DEFAULT_MAX_DATAGRAM_BYTES,
            udp_cookie_key: None,
            udp_congestion: UdpCongestionKind::Off,
            udp_migration: None,
            udp_security: UdpSecurityKind::Sealed,
            udp_static_key: None,
            udp_static_key_file: None,
            udp_handshakes_per_sec: Some(gsb_net::udp::DEFAULT_HANDSHAKES_PER_SEC),
            tls_cert: String::new(),
            tls_key: String::new(),
            listeners: None,
            listen_backlog: gsb_net::listen::DEFAULT_LISTEN_BACKLOG,
            max_handshakes_per_source: None,
            max_unauth_conns_per_source: None,
            udp_recv_buffer_bytes: None,
            udp_send_buffer_bytes: None,
            aoi_cell_size: 20.0,
            team_vision_radius: DEMO_DEFAULT_VISION_RADIUS,
            spawn_half_size: DEMO_DEFAULT_SPAWN_HALF,
            disconnect_grace_secs: DEMO_DEFAULT_DISCONNECT_GRACE_SECS,
            http_listen: String::new(),
            http_max_connections: Some(DEFAULT_HTTP_MAX_CONNECTIONS),
            http_write_timeout_secs: DEFAULT_HTTP_WRITE_TIMEOUT_SECS,
            http_route_timeout_secs: DEFAULT_HTTP_ROUTE_TIMEOUT_SECS,
            metrics: MetricsConfig::default(),
            game: crate::games::DEFAULT_GAME.into(),
            raw: toml::Table::new(),
            origin: crate::config::ConfigOrigin::default(),
        }
    }
}

impl Config {
    /// Load configuration from a TOML file.
    pub fn from_file(path: impl AsRef<std::path::Path>) -> Result<Self, ConfigError> {
        let path = path.as_ref();
        let text = std::fs::read_to_string(path).map_err(|e| ConfigError::Io {
            path: path.display().to_string(),
            source: e,
        })?;
        let mut cfg: Self = toml::from_str(&text).map_err(|e| ConfigError::Parse {
            path: path.display().to_string(),
            source: e,
        })?;
        // Config-file convenience: an explicit 0 means "unlimited" for the
        // caps (a cap of 0 would be a room/server nobody can enter). This
        // mirrors the loadgen CLI semantics (`--max-players 0` etc.).
        // Omitting the key keeps the built-in default (see `Default`);
        // `idle_timeout_secs = 0` / `write_stall_secs = 0` are already
        // handled at use time.
        if cfg.max_players == Some(0) {
            cfg.max_players = None;
        }
        if cfg.max_connections == Some(0) {
            cfg.max_connections = None;
        }
        // The same text as a plain table, for the game module (see
        // `Config::raw`); it parsed as a `Config` just above.
        cfg.raw = toml::from_str(&text).map_err(|e| ConfigError::Parse {
            path: path.display().to_string(),
            source: e,
        })?;
        // Where each top-level key was written, while the text is here
        // (see `Config::origin`; BACKLOG F64).
        cfg.origin = crate::config::ConfigOrigin::of_file(path.display().to_string(), &text);
        Ok(cfg)
    }
}
