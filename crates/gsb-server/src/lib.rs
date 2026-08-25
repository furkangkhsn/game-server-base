//! gsb composition root.
//!
//! [`start_server`] wires the whole stack together: the transport (default
//! TCP, pluggable), the registry actor (control plane), the pre-created
//! rooms (via the game crate's [`gsb_game::room::OpenRoom`]), and the accept
//! loop. It must be called from inside a tokio runtime.
//!
//! ```text
//! global ticker (broadcast) ──TickInfo──▶ room actors (5-phase tick)
//!                                            │ per-conn action channels (in)
//! accept loop ──ConnOpened──▶ registry actor ◀──RegistryMsg── connection actors
//!     │                           │CreateRoom/SpawnPlayer/…      │ try_send
//!     ▼                           ▼                               ▼
//! endpoint pumps             (control plane)              room actor (pulls)
//! (reader/writer per conn)                               │
//!                                                        ▼
//!                                         World (bevy_ecs) + RoomLogic (gsb-game)
//! ```

use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use bevy_ecs::world::World;
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tracing::{info, warn};

mod http;

use gsb_core::channel::{channel, Inbox, Mailbox};
use gsb_core::conn::ConnectionActor;
use gsb_core::error::CoreError;
use gsb_core::id::{ConnectionId, RoomId};
use gsb_core::metrics::{MetricReport, MetricSink, MetricsCollector, MetricsEvent};
use gsb_core::registry::{BuiltRoom, MatchResult, Registry, RegistryMsg, RoomFactory, RoomStatus};
use gsb_core::room::{RoomConfig, RoomLogic};
use gsb_core::auth::TicketAuth;

/// The LEGACY config spelling of two of the three selection axes
/// (`docs/ROADMAP.md`, P2 "Konfigürasyon düzeltmesi"): the demo rooms'
/// visibility strategy (config-selectable; all run
/// the SAME game — same components, movement, wire format — and differ
/// only in how the world is partitioned into snapshot groups, see
/// `docs/DESIGN.md` §8).
///
/// WHY it survives unchanged: backward compatibility — every pre-axes
/// config file, test and caller encodes its choice in this one key. It is
/// an INPUT ENCODING only: [`Config::resolve_selection`] decodes it into
/// the authoritative axes ([`Topology`] × [`VisibilityAxis`] ×
/// [`Communication`]) before anything runs, so no factory ever matches on
/// this enum again. Its `"sharded"` variant is really a topology
/// statement, which the decode makes explicit.
///
/// Each variant is a different `RoomLogic` group key, so each needs its
/// own registry instantiation (a `RoomFactory` is generic over the group
/// key — the pick happens at this config boundary, entirely on the
/// server side; `gsb-core` stays generic and untouched).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Visibility {
    /// `GroupKey = ()`: everyone sees the whole world. The baseline
    /// (per-connection bandwidth O(entities)); the comparison point all
    /// other strategies are measured against.
    All,
    /// `GroupKey = Cell`: spatial AOI (3×3 cell block, see
    /// [`gsb_game::aoi`]).
    Spatial,
    /// `GroupKey = Team`: team fog of war (2 groups, team vision; see
    /// [`gsb_game::team`]).
    Team,
    /// `GroupKey = Sector`: per-map-segment PVS (static visibility table
    /// over hand-authored convex sectors; see [`gsb_game::pvs`]).
    Pvs,
    /// Grid of shards (see [`gsb_game::sharded`]): the room is `shard_count`
    /// actors, each owning a rectangular region of the map, with entity
    /// migration across region boundaries and boundary visibility. This is
    /// a different *topology* (N actors + N worlds), not just a group key,
    /// so it plugs in via `ShardLogic`/`BuiltRoom::Sharded` rather than
    /// `RoomLogic`. Selecting it uses [`Config::shard_count`] (1..=256).
    Sharded,
}

impl Default for Visibility {
    /// Default: `All` — the whole-world baseline. Rationale (see
    /// `docs/ROADMAP.md`, visibility turn): (1) it is the backward-
    /// compatible behavior of every existing config (previously
    /// `aoi = false`); (2) it is the measurement baseline all restricted
    /// strategies are compared against — a restricted default would make
    /// the baseline opt-in; (3) for a new server author, "everyone sees
    /// everything" is the least-surprising starting point: restricting
    /// what clients can see is a *gameplay* decision and should be an
    /// explicit choice, not a default.
    fn default() -> Self {
        Self::All
    }
}

impl std::fmt::Display for Visibility {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            Self::All => "all",
            Self::Spatial => "spatial",
            Self::Team => "team",
            Self::Pvs => "pvs",
            Self::Sharded => "sharded",
        };
        f.write_str(s)
    }
}

/// The TOPOLOGY axis of the three-axis room selection (`docs/ROADMAP.md`,
/// P2 "Konfigürasyon düzeltmesi"): who computes the world, and as how many
/// authoritative pieces. Orthogonal to WHAT a group is ([`VisibilityAxis`])
/// and HOW snapshots are packaged ([`Communication`]).
///
/// A separate key from the legacy [`Visibility`] spelling because
/// `visibility = "sharded"` was never a visibility statement — it changed
/// the ACTOR/OWNERSHIP structure (N shard actors + N worlds instead of one).
/// The new key names that concept directly; the legacy spelling keeps
/// working as an input encoding (see [`Config::resolve_selection`]).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Topology {
    /// ONE room actor over ONE world — every shipped strategy except the
    /// grid (the default, and the behavior of every pre-axes config).
    #[default]
    Single,
    /// The map is cut into a `Config::shard_count`-cell grid; each shard is
    /// its own actor + world with entity migration across the seams (see
    /// `gsb_game::sharded`). Requires the count to be 1..=256.
    Sharded,
}

impl std::fmt::Display for Topology {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            Self::Single => "single",
            Self::Sharded => "sharded",
        };
        f.write_str(s)
    }
}

/// The COMMUNICATION axis: how snapshot data is packaged and carried to a
/// client — a full frame every time, or deltas converging on the keepalive
/// full (`docs/ROADMAP.md`, P2; the N-delta + 1-full convergence rule).
///
/// WHY the axis exists even though only one room serves client-facing
/// delta yet: the spatial room already diffs per cell INTERNALLY, so the
/// axis records a real distinction the roadmap generalizes (per-link
/// derivation, Faz C). Under `single × spatial` an explicit request
/// resolves to that same AoiRoom; everywhere else it fails at startup
/// with an error pointing at the phase that will deliver it — a request
/// is never silently downgraded.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Communication {
    /// Every snapshot frame carries the group's full state — what every
    /// shipped room speaks on the wire today (the default).
    #[default]
    AlwaysFull,
    /// Client-facing delta frames with periodic/full keepalive
    /// convergence. Served today ONLY by `single × spatial` (AoiRoom's
    /// internal per-cell diff): every other combination — all/team/pvs on
    /// `single`, anything on `sharded` — refuses startup with an error
    /// naming the roadmap phase that will deliver it.
    Delta,
}

impl std::fmt::Display for Communication {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            Self::AlwaysFull => "always-full",
            Self::Delta => "delta",
        };
        f.write_str(s)
    }
}

/// The resolved VISIBILITY axis: within the world, WHO sees WHOM — the
/// group-key choice of the room that will run (`docs/DESIGN.md` §8).
///
/// Deliberately NOT the legacy [`Visibility`] enum: that one fuses two
/// axes (its `"sharded"` spelling is really a [`Topology`] statement), so
/// a resolved selection carrying it could name impossible states (a
/// "sharded group key"). This axis is produced only by
/// [`Config::resolve_selection`]; there is no separate TOML key — the
/// legacy `visibility` key doubles as its input encoding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VisibilityAxis {
    /// `GroupKey = ()`: everyone sees the whole world (the baseline).
    All,
    /// `GroupKey = Cell`: spatial AOI, 3×3 cell block (see
    /// `gsb_game::aoi`).
    Spatial,
    /// `GroupKey = Team`: team fog of war, 2 groups (see
    /// `gsb_game::team`).
    Team,
    /// `GroupKey = Sector`: static PVS over hand-authored convex sectors
    /// (see `gsb_game::pvs`).
    Pvs,
}

impl From<Visibility> for VisibilityAxis {
    /// Decode the legacy five-value spelling into the axis. `"sharded"`
    /// folds into [`VisibilityAxis::All`]: by the time this conversion
    /// runs, the topology half of the spelling has already been extracted
    /// (see [`Config::resolve_selection`]), and a shard grid's groups are
    /// whole-world-per-shard today.
    fn from(legacy: Visibility) -> Self {
        match legacy {
            Visibility::All | Visibility::Sharded => Self::All,
            Visibility::Spatial => Self::Spatial,
            Visibility::Team => Self::Team,
            Visibility::Pvs => Self::Pvs,
        }
    }
}

impl std::fmt::Display for VisibilityAxis {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            Self::All => "all",
            Self::Spatial => "spatial",
            Self::Team => "team",
            Self::Pvs => "pvs",
        };
        f.write_str(s)
    }
}

/// The concrete room build a validated selection maps onto: one variant
/// per factory that exists TODAY (no new strategies — Faz A only re-expresses
/// the old surface). Exposed so tests and tooling can assert WHICH room a
/// config resolves to without starting a server.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RoomKind {
    /// `gsb_game::room::OpenRoom` — single actor, whole-world groups
    /// (open visibility: everyone sees everything).
    Open,
    /// `gsb_game::aoi::AoiRoom` — single actor, spatial AOI cells.
    Aoi,
    /// `gsb_game::team::TeamRoom` — single actor, team fog of war.
    Team,
    /// `gsb_game::pvs::SectorRoom` — single actor, sector PVS.
    Sector,
    /// `gsb_game::sharded::ShardedRoom` grid — N shard actors, whole-world
    /// groups per shard (`BuiltRoom::Sharded`).
    Sharded,
}

/// The fully-resolved three-axis selection: the raw config surface
/// (legacy spellings + explicit keys) reduced to validated axes plus the
/// room build they map onto. Produced ONLY by
/// [`Config::resolve_selection`] — everything downstream of it (the
/// factory pick in `start_inner`) reads THIS, never the legacy string, so
/// the axes are authoritative and the old spellings are mere encodings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResolvedSelection {
    /// Who computes the world.
    pub topology: Topology,
    /// Who sees whom within it.
    pub visibility: VisibilityAxis,
    /// How snapshots are packaged for clients.
    pub communication: Communication,
    /// The existing factory this triple resolves to.
    pub kind: RoomKind,
}
use gsb_net::tcp::TcpTransport;
use gsb_net::tls::{TlsTransport, TlsTransportConfig};
use gsb_net::transport::Transport;
use gsb_net::udp::{UdpTransport, UdpTransportConfig};
use gsb_protocol::MessageTable;

/// The wire transport (see `config.example.toml` and `docs/DESIGN.md` §6).
///
/// Both transports sit behind the same `gsb_net::transport` traits, so
/// the actor layer is identical for either; they differ in the wire
/// protocol and the session lifecycle:
///
/// - `Tcp`: one socket per connection, length-prefixed frames, stream
///   semantics (the reader pump's idle timeout is the teardown guard).
/// - `Udp`: rUDP — one socket for every session, a stateless cookie
///   handshake (anti-amplification), a reliable control band over a
///   loss-tolerant snapshot band, a datagram budget, and idle teardown
///   via the demux's deadline heap (no FIN in UDP). See `gsb_net::udp`.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TransportKind {
    /// Length-prefixed TCP (the default).
    #[default]
    Tcp,
    /// rUDP (one shared socket, one shared demux).
    Udp,
}

impl std::fmt::Display for TransportKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            Self::Tcp => "tcp",
            Self::Udp => "udp",
        };
        f.write_str(s)
    }
}

/// The per-listener transport spelling inside a `[[listeners]]` entry.
///
/// WHY a separate enum from [`TransportKind`] instead of a `Tls` variant
/// there: TLS is NOT a distinct wire framing — it is TCP with a rustls
/// upgrade (the accept loop, pumps and actors cannot tell them apart), so
/// the legacy scalar key keeps encoding it as `transport = "tcp"` plus the
/// cert/key pair. A listener ARRAY needs to name the three *deployments*
/// unambiguously in one key ("tls" carries its own cert/key paths per
/// entry), and reusing `TransportKind` would silently widen the legacy
/// scalar grammar (`transport = "tls"` would start parsing where it used
/// to be a config error). Two small enums keep each grammar exactly as
/// wide as it was.
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
}

impl std::fmt::Display for ListenerTransport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            Self::Tcp => "tcp",
            Self::Tls => "tls",
            Self::Udp => "udp",
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
#[derive(Debug, Clone, serde::Deserialize)]
pub struct ListenerEntry {
    /// Which transport this listener serves (`"tcp"`, `"tls"`, `"udp"`).
    pub transport: ListenerTransport,
    /// Socket address to bind (e.g. `"0.0.0.0:7777"`, `"127.0.0.1:0"`).
    /// Required: an unnamed door is a config mistake, not a default.
    pub bind: String,
    /// Path to the PEM certificate chain (leaf first) — REQUIRED (with
    /// `tls_key`) when `transport = "tls"`; forbidden otherwise (a "tcp"
    /// or "udp" entry carrying TLS files is a startup error, never a
    /// silent reinterpretation of the entry).
    pub tls_cert: Option<String>,
    /// Path to the PEM private key matching [`Self::tls_cert`] — see there.
    pub tls_key: Option<String>,
}

/// Server configuration (see `config.example.toml`).
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
    ///   against the built-in default base ([`DEFAULT_MAX_CONNECTIONS`],
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
    /// (rUDP MTU readiness; default = `max_frame_bytes`).
    pub max_snapshot_bytes: usize,
    /// Keep-alive rate for unchanged snapshot groups, in Hz (a client that
    /// lost its last snapshot must not stay stale forever). `<= 0` disables.
    pub keepalive_hz: f64,
    /// The TOPOLOGY selection axis (`"single"` | `"sharded"`; see
    /// [`Topology`]): who computes the world and as how many authoritative
    /// pieces.
    ///
    /// Value semantics (resolved ONCE at startup, see
    /// [`Self::resolve_selection`]):
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
    /// [`Self::resolve_selection`]):
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
    /// The four non-`"sharded"` spellings ARE the [`VisibilityAxis`]
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
    /// ¹ names the packaging the spatial room ALREADY serves (its
    /// internal per-cell diff): the explicit [`Self::communication`] key
    /// resolves to the same room there; outside `single × spatial` an
    /// explicit `"delta"` is rejected until the common codec ships.
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
    /// shard_count`, see [`gsb_game::sharded::grid_shape`]). Must be
    /// 1..=256 (the grid topology); validated at startup. Default 4 (2×2).
    pub shard_count: u32,
    /// World units per AOI cell edge (used only when
    /// [`Self::visibility`] = `Spatial`). See `gsb_game::aoi` for the
    /// `max_snapshot_bytes` / density relation and the measured break-even.
    /// The transport (see [`TransportKind`]).
    pub transport: TransportKind,
    /// The rUDP datagram budget in bytes (transport-level guard; default
    /// 1472 = MTU 1500 − IP 20 − UDP 8). Used only when
    /// [`Self::transport`] = `Udp`. See `gsb_net::udp` (feature 3:
    /// oversized frames are dropped and counted, never fragmented).
    pub udp_max_datagram_bytes: usize,
    /// The rUDP cookie key as 32 hex characters (16 bytes). Used only
    /// when [`Self::transport`] = `Udp`. `None` (the default) = draw the
    /// key from the OS entropy source at bind time; if that draw fails
    /// the server refuses to start — a predictable key would invert the
    /// handshake's anti-amplification property (see `gsb_net::udp`).
    pub udp_cookie_key: Option<String>,
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
    /// paying clients next to a plain-TCP door for a LAN build, or an rUDP
    /// door beside both. Every accepted endpoint flows into the SAME
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
    /// World units per AOI cell edge (used only when
    /// [`Self::visibility`] = `Spatial`). See `gsb_game::aoi` for the
    /// `max_snapshot_bytes` / density relation and the measured break-even.
    pub aoi_cell_size: f32,
    /// World units an enemy must be within to be visible to a team (used
    /// only when [`Self::visibility`] = `Team`). See `gsb_game::team` for
    /// the vision source model.
    pub team_vision_radius: f32,
    /// Half-size of the square map entities spawn on (all four demo rooms;
    /// default 50 = the historical 100×100 arena, bit-identical). A load
    /// profile that places entities on a "wide map" (the generator's
    /// `spread` profile) pairs a large value here with the same value in
    /// the clients' `--spawn-half-size`, so spawn points and targets live
    /// on the same map and the run is statistically steady from tick 1.
    /// The PVS strategy's *visibility map* stays its hand-authored 100×100
    /// sectors regardless (see `gsb_game::pvs::SectorRoom::spawn_half`).
    pub spawn_half_size: f32,
    /// The demo rooms' disconnect-park grace, in seconds (`config.example.toml`:
    /// `disconnect_grace_secs`; RECONNECT §3): how long a dropped
    /// transport's entity STAYS in the world — visible in snapshots,
    /// holding its room-cap slot — before the hold ends toward the bot
    /// handover ([`ExpireTo::AiHandover`]; the demo stub bot then keeps
    /// playing the hero through the ordinary input path). A human who
    /// rejoins inside the window resumes onto the live entity with the
    /// same wire id (the implicit resume, §14.3).
    ///
    /// Default 30 s; `0` restores the pre-reconnect semantics exactly
    /// (disconnect = despawn). Flows into every room the factories build
    /// (the game-level knob rides the factory closure like
    /// `spawn_half_size`, not [`RoomConfig`] — it is policy, not core
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
}

/// The built-in default server-wide connection cap (DESIGN §1's design
/// goal as a guardrail). Single source of truth for the `Config` default
/// AND the unauth-cap derivation when `max_connections` is unlimited.
const DEFAULT_MAX_CONNECTIONS: u64 = 100_000;

/// The floor of the derived unauthenticated-connection cap
/// (`unauth_cap_of`): even the smallest deployment gets real headroom for
/// slow-but-honest handshakes instead of a cap that rounds to near-zero.
const MIN_UNAUTH_CONNS: u64 = 64;

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
            max_players: Some(10_000),
            max_connections: Some(DEFAULT_MAX_CONNECTIONS),
            max_unauth_conns: None,
            max_snapshot_bytes: gsb_net::tcp::DEFAULT_MAX_FRAME_BYTES,
            keepalive_hz: 1.0,
            topology: None,
            communication: None,
            visibility: Visibility::default(),
            shard_count: 4,
            transport: TransportKind::default(),
            udp_max_datagram_bytes: gsb_net::udp::DEFAULT_MAX_DATAGRAM_BYTES,
            udp_cookie_key: None,
            tls_cert: String::new(),
            tls_key: String::new(),
            listeners: None,
            aoi_cell_size: 20.0,
            team_vision_radius: gsb_game::team::DEFAULT_VISION_RADIUS,
            spawn_half_size: gsb_game::room::DEFAULT_SPAWN_HALF,
            disconnect_grace_secs: gsb_game::DEFAULT_DISCONNECT_GRACE.as_secs_f64(),
            http_listen: String::new(),
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
        // `idle_timeout_secs = 0` is already handled at use time.
        if cfg.max_players == Some(0) {
            cfg.max_players = None;
        }
        if cfg.max_connections == Some(0) {
            cfg.max_connections = None;
        }
        Ok(cfg)
    }

    /// Reduce the raw config surface to the validated three-axis selection
    /// (topology × visibility × communication — `docs/ROADMAP.md`, P2
    /// "Konfigürasyon düzeltmesi", Faz A) and map it onto the room build
    /// that will run.
    ///
    /// This is THE gate between "what the operator wrote" and "what will
    /// run": the composition root matches on the returned
    /// [`ResolvedSelection::kind`] instead of the raw legacy string, so
    /// the axes are authoritative and legacy spellings stay input
    /// encodings. Derivation + precedence:
    ///
    /// 1. TOPOLOGY — explicit [`Self::topology`] wins; omission derives
    ///    from the legacy encoding (`visibility = "sharded"` ⇒ sharded).
    /// 2. VISIBILITY — decoded from the legacy [`Self::visibility`] key
    ///    (`"sharded"` folds into `all`; its topology half was taken in
    ///    step 1).
    /// 3. COMMUNICATION — explicit [`Self::communication`] wins; omission
    ///    derives `spatial ⇒ delta, otherwise always-full` (what today's
    ///    rooms actually do).
    ///
    /// Combination validation runs on the RESOLVED triple. Only six
    /// combinations have an implementation today (single × {all, team,
    /// pvs} × always-full, single × spatial × {always-full, delta}, and
    /// sharded × all × always-full); everything else is rejected HERE
    /// with an error naming the roadmap phase/document that will deliver
    /// it — a supported-combination check must refuse at startup, never
    /// misconfigure a running server.
    ///
    /// One exemption on the communication axis: under
    /// `single × spatial`, `delta` names the packaging AoiRoom ALREADY
    /// serves (its internal per-cell diff), so BOTH spellings resolve to
    /// that same room — the derived one (legacy `visibility = "spatial"`,
    /// key omitted) and an explicit `communication = "delta"` request
    /// alike. Same room, one behavior; two spellings must not disagree.
    /// Everywhere else an explicit `communication = "delta"` requests
    /// client-facing delta frames nothing serves yet (all/team/pvs on
    /// single; anything on sharded) and is rejected.
    pub fn resolve_selection(&self) -> Result<ResolvedSelection, ServerError> {
        // Stage 1 — TOPOLOGY: explicit key wins over the legacy spelling;
        // a contradiction warns (behavior still follows the explicit key).
        let legacy_sharded = self.visibility == Visibility::Sharded;
        let topology = match self.topology {
            Some(explicit) => {
                if legacy_sharded && explicit == Topology::Single {
                    warn!(
                        resolved = %explicit,
                        "`topology` takes precedence: ignoring the legacy \
                         visibility = \"sharded\" spelling"
                    );
                }
                explicit
            }
            None if legacy_sharded => Topology::Sharded,
            None => Topology::Single,
        };

        // Stage 2 — VISIBILITY axis: decode the legacy five-value spelling.
        let visibility = VisibilityAxis::from(self.visibility);

        // Stage 3 — COMMUNICATION: explicit key wins over the derived
        // default (the default mirrors what the mapped room does today).
        let derived_communication = match visibility {
            VisibilityAxis::Spatial => Communication::Delta,
            VisibilityAxis::All | VisibilityAxis::Team | VisibilityAxis::Pvs => {
                Communication::AlwaysFull
            }
        };
        let communication = self.communication.unwrap_or(derived_communication);

        // Stage 4 — combination validation, structural axes first (they
        // decide what the world IS), then the packaging axis. Each
        // rejection names the roadmap phase/document that delivers it.
        let kind = match (topology, visibility) {
            (Topology::Single, VisibilityAxis::All) => RoomKind::Open,
            (Topology::Single, VisibilityAxis::Spatial) => RoomKind::Aoi,
            (Topology::Single, VisibilityAxis::Team) => RoomKind::Team,
            (Topology::Single, VisibilityAxis::Pvs) => RoomKind::Sector,
            (Topology::Sharded, VisibilityAxis::All) => RoomKind::Sharded,
            // No implementation yet: every shard diffusing its own cell
            // groups is the Faz B composite. Fail cleanly instead of
            // silently running whole-shard snapshots under a config that
            // asked for per-cell AOI across the grid.
            (Topology::Sharded, VisibilityAxis::Spatial) => return Err(ServerError::ShardedSpatial),
            // Locality-contrary combos: team/pvs interest reaches across
            // shard seams, which needs a cross-shard subscription layer
            // nobody has built (see docs/CROSS-SHARD.md §4 — interaction
            // designs stay shard-local; docs/DISTRIBUTED.md horizon item).
            (Topology::Sharded, other @ (VisibilityAxis::Team | VisibilityAxis::Pvs)) => {
                return Err(ServerError::ShardedCrossInterest(other.to_string()))
            }
        };

        // An EXPLICIT delta request resolves only where a client-facing
        // delta implementation exists TODAY: single × spatial is served by
        // AoiRoom (its internal per-cell diff IS the delta packaging), so
        // the explicit spelling must land on the same room as the derived
        // one — rejecting it there while accepting the derived spelling of
        // the identical triple would make two names for one room disagree.
        // Everywhere else (all/team/pvs on single, anything on sharded)
        // delta frames wait for the common codec (single) and for the
        // per-shard delta book / per-link derivation (sharded): fail
        // cleanly instead of silently serving full frames under a config
        // that asked for deltas. Sharded × spatial never reaches this arm
        // (rejected above), so a sharded rejection here implies all.
        if self.communication == Some(Communication::Delta)
            && !(topology == Topology::Single && visibility == VisibilityAxis::Spatial)
        {
            return Err(match topology {
                Topology::Single => ServerError::SingleDelta,
                Topology::Sharded => ServerError::ShardedDelta,
            });
        }

        Ok(ResolvedSelection {
            topology,
            visibility,
            communication,
            kind,
        })
    }
}

/// Configuration errors.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("cannot read config file {path}: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },

    #[error("cannot parse config file {path}: {source}")]
    Parse {
        path: String,
        #[source]
        source: toml::de::Error,
    },
}

/// Errors produced while starting the server.
#[derive(Debug, thiserror::Error)]
pub enum ServerError {
    #[error("invalid bind address `{0}`: {1}")]
    BadBind(String, String),

    #[error("transport bind failed: {0}")]
    Bind(#[from] std::io::Error),

    #[error("invalid `udp_cookie_key` in config: {0}")]
    BadCookieKey(String),

    #[error("invalid `shard_count` {0}: must be 1..=256 (grid topology)")]
    BadShardCount(u32),

    #[error("topology = \"sharded\" with visibility = \"spatial\" has no \
             implementation yet: per-shard cell-grouped broadcast is \
             ROADMAP Faz B (the sharded × spatial composite); use \
             visibility = \"all\" today")]
    ShardedSpatial,

    #[error("topology = \"sharded\" with visibility = \"{0}\" breaks shard \
             locality: cross-shard interest needs a subscription layer \
             that does not exist yet (docs/CROSS-SHARD.md §4 keeps every \
             interaction design shard-local); run team/pvs rooms on \
             topology = \"single\"")]
    ShardedCrossInterest(String),

    #[error("communication = \"delta\" with topology = \"single\" needs a \
             visibility that serves delta today: only spatial (AoiRoom) \
             does; all/team/pvs have no delta packaging yet (ROADMAP: \
             ortak DeltaSnapshotCodec) — use visibility = \"spatial\" or \
             communication = \"always-full\"")]
    SingleDelta,

    #[error("communication = \"delta\" with topology = \"sharded\" has no \
             implementation yet — the per-shard delta book lands with \
             ROADMAP Faz B and per-link communication derivation with \
             ROADMAP Faz C; use communication = \"always-full\"")]
    ShardedDelta,

    #[error("invalid `tick_hz` {0}: must be finite and > 0 (the global ticker derives its period as 1/hz; a rate without a period refuses startup instead of panicking)")]
    BadTickRate(f64),

    #[error("invalid `http_listen` address `{0}`: {1}")]
    BadHttpListen(String, String),

    #[error("`tls_cert` is set but `tls_key` is empty: TLS needs BOTH files; \
             refusing to start half-configured (a silent plaintext fallback \
             would hide the mistake) — docs/SECURITY.md §2 decision 3")]
    TlsCertNeedsKey,

    #[error("`tls_key` is set but `tls_cert` is empty: TLS needs BOTH files; \
             refusing to start half-configured — docs/SECURITY.md §2 decision 3")]
    TlsKeyNeedsCert,

    #[error("transport = \"udp\" cannot be combined with tls_cert/tls_key: \
             rUDP is experimental and takes no TLS (its cookie handshake is \
             its own anti-amplification boundary) — docs/SECURITY.md §2 \
             decision 7")]
    UdpWithTls,

    #[error("`[[listeners]]` is present but empty: a server with no listener \
             cannot accept clients; remove the empty table to fall back to \
             the legacy scalar keys, or add entries — never an implicit \
             fallback that hides a half-edited config")]
    EmptyListeners,

    #[error("duplicate bind address `{0}` in `[[listeners]]`: two listeners \
             cannot own one address (the second bind would fail anyway; \
             reporting it at config time names the culprit instead of \
             failing inside a bind syscall)")]
    DuplicateBind(String),

    #[error("`[[listeners]]` entry `{bind}` sets transport = \"tls\" with \
             tls_cert but no tls_key: TLS needs BOTH files; refusing to \
             start half-configured — docs/SECURITY.md §2 decision 3")]
    ListenerTlsCertNeedsKey {
        /// The offending entry's bind address (names the entry in logs).
        bind: String,
    },

    #[error("`[[listeners]]` entry `{bind}` sets transport = \"tls\" with \
             tls_key but no tls_cert: TLS needs BOTH files; refusing to \
             start half-configured — docs/SECURITY.md §2 decision 3")]
    ListenerTlsKeyNeedsCert {
        /// The offending entry's bind address (names the entry in logs).
        bind: String,
    },

    #[error("`[[listeners]]` entry `{bind}`: transport = \"tcp\" cannot carry \
             tls_cert/tls_key — write transport = \"tls\" for an encrypted \
             door (a plaintext door with TLS files attached is a config \
             mistake, not something to silently reinterpret)")]
    ListenerTcpWithTls {
        /// The offending entry's bind address (names the entry in logs).
        bind: String,
    },

    #[error("`[[listeners]]` entry `{bind}`: transport = \"udp\" cannot be \
             combined with tls_cert/tls_key: rUDP is experimental and takes \
             no TLS — docs/SECURITY.md §2 decision 7")]
    ListenerUdpWithTls {
        /// The offending entry's bind address (names the entry in logs).
        bind: String,
    },
}

/// One fully-validated listener, ready to bind: the config grammar
/// (`ListenerEntry`, legacy scalars) has been reduced to a transport
/// instance recipe plus its parsed socket address. Built once by
/// [`resolve_listeners`] BEFORE anything binds, so a bad config fails at
/// startup without half-starting (the same principle as the legacy
/// pre-bind TLS checks).
pub(crate) enum ListenerSpec {
    /// Plaintext length-prefixed TCP.
    Tcp { addr: SocketAddr },
    /// TCP + rustls with these PEM files (loaded at bind time).
    Tls {
        addr: SocketAddr,
        cert_pem: String,
        key_pem: String,
    },
    /// rUDP; global udp knobs apply (`udp_max_datagram_bytes`,
    /// `udp_cookie_key`) — see `ListenerEntry` for why they stay global.
    Udp { addr: SocketAddr },
}

impl ListenerSpec {
    /// The parsed address this listener will claim.
    fn addr(&self) -> SocketAddr {
        match self {
            Self::Tcp { addr }
            | Self::Tls { addr, .. }
            | Self::Udp { addr } => *addr,
        }
    }
}

/// Reduce the config's listener surface to validated [`ListenerSpec`]s:
///
/// - `listeners` absent → ONE spec derived from the legacy scalar keys,
///   preserving their exact behavior INCLUDING their error variants
///   (half-set `tls_*` → `TlsCertNeedsKey`/`TlsKeyNeedsCert`; `udp` +
///   `tls_*` → `UdpWithTls`), so existing configs keep failing in exactly
///   the ways they always did;
/// - `listeners` non-empty → one spec per entry, each validated on its own
///   ("tls" needs both files; "tcp"/"udp" take none), with a warn when any
///   legacy scalar was also touched (see [`Config::listeners`]);
/// - `listeners` empty → `EmptyListeners`.
///
/// Duplicates are detected over PARSED addresses (not raw strings).
/// Concrete addresses must be unique across entries; port 0 is exempt (see
/// Stage 3 below) because each `:0` entry asks the OS for its OWN free
/// port.
fn resolve_listeners(cfg: &Config) -> Result<Vec<ListenerSpec>, ServerError> {
    // Stage 1 — reduce BOTH config grammars to one internal shape: the
    // entry's transport kind, its (still unparsed) bind string, and its
    // optional TLS files. The tls-file COMBINATION checks are grammar-level
    // policy, so they run here, per entry.
    let mut entries: Vec<(ListenerTransport, String, Option<String>, Option<String>)> =
        match &cfg.listeners {
            Some(entries) if entries.is_empty() => return Err(ServerError::EmptyListeners),
            Some(entries) => {
                // Prefer-the-array warning: fire only when a legacy scalar
                // actually differs from its built-in default. The deserializer
                // cannot tell "explicitly set to the default value" from
                // "omitted", so an operator who left every scalar alone gets
                // no noise; one who set both sees which side won.
                let def = Config::default();
                let legacy_touched = cfg.bind != def.bind
                    || cfg.transport != def.transport
                    || cfg.tls_cert != def.tls_cert
                    || cfg.tls_key != def.tls_key;
                if legacy_touched {
                    warn!(
                        entries = entries.len(),
                        "`[[listeners]]` takes precedence: ignoring the legacy                          scalar transport keys (transport/bind/tls_cert/tls_key)"
                    );
                }
                entries
                    .iter()
                    .map(|e| {
                        match e.transport {
                            ListenerTransport::Tcp => {
                                if e.tls_cert.is_some() || e.tls_key.is_some() {
                                    return Err(ServerError::ListenerTcpWithTls {
                                        bind: e.bind.clone(),
                                    });
                                }
                            }
                            ListenerTransport::Tls => {
                                if e.tls_cert.is_none() {
                                    return Err(ServerError::ListenerTlsCertNeedsKey {
                                        bind: e.bind.clone(),
                                    });
                                }
                                if e.tls_key.is_none() {
                                    return Err(ServerError::ListenerTlsKeyNeedsCert {
                                        bind: e.bind.clone(),
                                    });
                                }
                            }
                            ListenerTransport::Udp => {
                                if e.tls_cert.is_some() || e.tls_key.is_some() {
                                    return Err(ServerError::ListenerUdpWithTls {
                                        bind: e.bind.clone(),
                                    });
                                }
                            }
                        }
                        Ok((
                            e.transport,
                            e.bind.clone(),
                            e.tls_cert.clone(),
                            e.tls_key.clone(),
                        ))
                    })
                    .collect::<Result<Vec<_>, ServerError>>()?
            }
            None => {
                // Legacy derivation: the single-scalar era's exact
                // semantics, INCLUDING its error variants, so existing
                // configs keep failing in exactly the ways they always did.
                match (cfg.tls_cert.is_empty(), cfg.tls_key.is_empty()) {
                    (true, true) | (false, false) => {}
                    (false, true) => return Err(ServerError::TlsCertNeedsKey),
                    (true, false) => return Err(ServerError::TlsKeyNeedsCert),
                }
                if cfg.transport == TransportKind::Udp && !cfg.tls_cert.is_empty() {
                    return Err(ServerError::UdpWithTls);
                }
                let (kind, cert, key) = match cfg.transport {
                    TransportKind::Udp => (ListenerTransport::Udp, None, None),
                    TransportKind::Tcp if !cfg.tls_cert.is_empty() => (
                        ListenerTransport::Tls,
                        Some(cfg.tls_cert.clone()),
                        Some(cfg.tls_key.clone()),
                    ),
                    TransportKind::Tcp => (ListenerTransport::Tcp, None, None),
                };
                vec![(kind, cfg.bind.clone(), cert, key)]
            }
        };

    // Stage 2 — parse every bind up front: a malformed address is a config
    // error that must fail BEFORE any socket exists (never half-start).
    let mut specs: Vec<ListenerSpec> = Vec::with_capacity(entries.len());
    for (kind, raw_bind, cert, key) in entries.drain(..) {
        let addr: SocketAddr = raw_bind.parse().map_err(
            |e: std::net::AddrParseError| ServerError::BadBind(raw_bind.clone(), e.to_string()),
        )?;
        let spec = match kind {
            ListenerTransport::Tcp => ListenerSpec::Tcp { addr },
            ListenerTransport::Tls => ListenerSpec::Tls {
                addr,
                cert_pem: cert.expect("tls entry validated to carry a cert path"),
                key_pem: key.expect("tls entry validated to carry a key path"),
            },
            ListenerTransport::Udp => ListenerSpec::Udp { addr },
        };
        specs.push(spec);
    }

    // Stage 3 — duplicate detection over PARSED addresses (not raw
    // strings): two doors claiming ONE concrete address is always a
    // mistake (the second bind could not succeed anyway), and reporting it
    // at config time names the offending entry instead of failing inside a
    // bind syscall. PORT 0 IS EXEMPT, on purpose: a configured `:0` is a
    // request for a different, OS-chosen free port EVERY time — two such
    // entries never end up on the same address, so treating their equal
    // spelling as a collision would make ephemeral-port deployments (and
    // every test suite) impossible while catching nothing real.
    let mut seen: std::collections::HashSet<SocketAddr> =
        std::collections::HashSet::with_capacity(specs.len());
    for spec in &specs {
        let addr = spec.addr();
        if addr.port() != 0 && !seen.insert(addr) {
            return Err(ServerError::DuplicateBind(addr.to_string()));
        }
    }
    Ok(specs)
}

/// Parse the config's 32-hex-char cookie key into 16 bytes (the rUDP
/// cookie key is an operator-supplied alternative to the OS-entropy
/// draw — see `gsb_net::udp::CookieKey`).
fn parse_cookie_key(s: &str) -> Result<[u8; 16], String> {
    if s.len() != 32 {
        return Err(format!("expected 32 hex characters, got {}", s.len()));
    }
    let mut out = [0u8; 16];
    for i in 0..16 {
        out[i] = u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).map_err(|_| {
            format!(
                "invalid hex pair `{}:{}` at position {}",
                &s[i * 2..i * 2 + 1],
                &s[i * 2 + 1..i * 2 + 2],
                i * 2
            )
        })?;
    }
    Ok(out)
}

/// The composition-root's platform hooks (the base ships the wiring, the
/// platform ships the behaviour):
///
/// - [`Self::ticket`]: the ticket-validation hook (`gsb_core::auth`) —
///   `None` (the default) = the legacy local-auth path, byte-for-byte
///   unchanged. The base defines the hook and deliberately implements
///   NO validator (a signature-service call, a cache, … is platform
///   specific — the same adapter split as the transport).
#[derive(Clone, Default)]
pub struct ServerHooks {
    /// The ticket-validation hook for the connection actors; `None` =
    /// local auth (`Auth.name` accepted as-is).
    pub ticket: Option<TicketAuth>,
}

// ── the control-plane round trips (shared) ─────────────────────────────
//
// The three room-lifecycle round trips below are the WHOLE of what both
// consumers need — `ServerHandle`'s programmatic API and the HTTP ops
// surface (`http.rs`) — so they live once, over a bare registry mailbox.
// WHY not on `ServerHandle`: the HTTP tasks hold only the mailbox (a cheap
// clone), not the join handles/listener a full handle carries.

/// Idempotent create (see [`RegistryMsg::CreateRoom`]); the reply doubles
/// as a status query.
pub(crate) async fn registry_open_room(
    registry: &Mailbox<RegistryMsg>,
    config: RoomConfig,
) -> Result<RoomStatus, CoreError> {
    let (tx, rx) = tokio::sync::oneshot::channel::<Result<RoomStatus, CoreError>>();
    registry
        .send(RegistryMsg::CreateRoom { config, reply: tx })
        .await
        .map_err(|_| CoreError::Io("registry gone".into()))?;
    rx.await.map_err(|_| CoreError::Io("registry dropped the reply".into()))?
}

/// Idempotent destroy (see [`RegistryMsg::DestroyRoom`]).
pub(crate) async fn registry_close_room(
    registry: &Mailbox<RegistryMsg>,
    id: RoomId,
) -> Result<RoomStatus, CoreError> {
    let (tx, rx) = tokio::sync::oneshot::channel::<RoomStatus>();
    registry
        .send(RegistryMsg::DestroyRoom { id, reply: tx })
        .await
        .map_err(|_| CoreError::Io("registry gone".into()))?;
    rx.await.map_err(|_| CoreError::Io("registry dropped the reply".into()))
}

/// Table-only status query (see [`RegistryMsg::RoomStatus`]).
pub(crate) async fn registry_room_status(
    registry: &Mailbox<RegistryMsg>,
    id: RoomId,
) -> Result<RoomStatus, CoreError> {
    let (tx, rx) = tokio::sync::oneshot::channel::<RoomStatus>();
    registry
        .send(RegistryMsg::RoomStatus { id, reply: tx })
        .await
        .map_err(|_| CoreError::Io("registry gone".into()))?;
    rx.await.map_err(|_| CoreError::Io("registry dropped the reply".into()))
}

/// Handle to a running server.
pub struct ServerHandle {
    registry: Mailbox<RegistryMsg>,
    /// One accept task PER listener (all sharing the pipeline below and
    /// one connection-id sequence). All are aborted on stop.
    accepts: Vec<JoinHandle<()>>,
    ticker: JoinHandle<()>,
    /// The metrics collector (emits one final report when the ticker's
    /// broadcast closes).
    metrics: JoinHandle<()>,
    /// The HTTP ops-surface task, when `http_listen` was configured.
    /// Aborted on stop (its listener drops with the aborted future).
    http: Option<JoinHandle<()>>,
    /// The bound listeners, in config order. `stop` closes each *before*
    /// aborting its accept task: for the rUDP transport this is what stops
    /// the shared demux task (a plain drop would not reach it — see
    /// `Listener::close`).
    listeners: Vec<Arc<dyn gsb_net::transport::Listener>>,
    /// The actual bound address of the FIRST listener (useful when binding
    /// port 0 in tests; kept for the legacy single-listener callers).
    pub addr: SocketAddr,
    /// The actual bound address of EVERY listener, in config order (the
    /// multi-listener shape; `addr` is `addrs[0]`).
    pub addrs: Vec<SocketAddr>,
    /// The actual bound address of the HTTP ops surface; `None` when the
    /// surface is disabled (`http_listen` empty — the default).
    pub http_addr: Option<SocketAddr>,
    /// The match-result sink (the control plane's result seam, see
    /// `gsb_core::room::RoomLogic::match_result`): the room's result
    /// arrives here on shutdown. The reference adapter is the
    /// composition root reading this receiver (one hop, in-process,
    /// bounded — no NATS/Kafka/gRPC in the base).
    pub match_results: Inbox<MatchResult>,
}

impl ServerHandle {
    /// Shut the server down: the registry tears down connections and rooms
    /// (rooms get a control `Shutdown`, processed on their next tick; a
    /// room with a configured result seam reports it on that shutdown); the
    /// ticker is aborted, which closes the broadcast and stops any room that
    /// missed its window; EVERY listener is closed (stopping any per-
    /// listener transport shared state, e.g. each rUDP demux); every accept
    /// loop is hard-aborted (documented v1 limitation). The HTTP ops surface
    /// is aborted with it. The metrics collector is awaited last: it emits
    /// one final report when the broadcast closes. The teardown ORDER is the
    /// single-listener order applied across all listeners: doors close
    /// first, so no new client can connect while the registry is tearing
    /// the existing ones down.
    pub async fn stop(self) {
        let _ = self.registry.send(RegistryMsg::Shutdown).await;
        if let Some(http) = self.http {
            http.abort();
        }
        self.ticker.abort();
        for l in &self.listeners {
            l.close();
        }
        for accept in &self.accepts {
            accept.abort();
        }
        let _ = self.metrics.await;
    }

    /// Open a room at runtime (the control plane's lifecycle API, feature
    /// A). **Idempotent**: creating a room that already exists with the
    /// IDENTICAL `config` is a no-op that reports the existing room's
    /// status (one room, not two — a resent request must not double the
    /// room); a different config for a live room is a
    /// [`CoreError::RoomConflict`]. The round trip doubles as a status
    /// query (the reply carries the [`RoomStatus`]).
    pub async fn open_room(&self, config: RoomConfig) -> Result<RoomStatus, CoreError> {
        registry_open_room(&self.registry, config).await
    }

    /// Close a room at runtime (feature A). **Idempotent**: closing a room
    /// that is not running is a no-op reported as [`RoomStatus::Absent`]
    /// (a control plane that retries a close never sees an error for the
    /// second attempt). The room stops on its next tick; connections
    /// affiliated with it are notified (`RoomGone`).
    pub async fn close_room(&self, id: RoomId) -> Result<RoomStatus, CoreError> {
        registry_close_room(&self.registry, id).await
    }

    /// Query a room's status (feature A): [`RoomStatus::Running`] (with
    /// the member count), or [`RoomStatus::Absent`]. The registry answers
    /// from its own table (it never awaits a room).
    pub async fn room_status(&self, id: RoomId) -> Result<RoomStatus, CoreError> {
        registry_room_status(&self.registry, id).await
    }
}

/// The demo composition: base protocol + demo game messages.
pub fn build_table() -> Arc<MessageTable> {
    let mut table = gsb_protocol::base_table();
    gsb_game::register(&mut table);
    Arc::new(table)
}

/// The open-visibility room factory: an empty bevy `World` + an
/// [`gsb_game::room::OpenRoom`] over a spawn map of half-size
/// `spawn_half`. Group key is `()` (one group per room) — the OPEN
/// strategy: everyone sees everything, every connection receives the
/// whole world (the unrestricted baseline the restricted visibility
/// strategies are measured against).
///
/// `economy` is the in-process economy service (the RPC pattern's
/// external-I/O reference adapter, see `gsb_game::economy`): ONE service
/// per server (a platform service, not a per-room one), shared by clone
/// with every room the factory builds.
fn open_room_factory(
    spawn_half: f32,
    disconnect_grace: std::time::Duration,
    economy: gsb_game::economy::EconomyService,
) -> RoomFactory<World, (), (), ()> {
    Arc::new(move |_id, _config| BuiltRoom::Single {
        world: World::new(),
        logic: Box::new(
            gsb_game::room::OpenRoom::with_spawn_half(spawn_half)
                .with_disconnect_grace(disconnect_grace)
                .with_economy(economy.clone()),
        )
            as Box<dyn RoomLogic<World, GroupKey = (), Strip = ()>>,
    })
}

/// The AOI room factory: an empty bevy `World` + an
/// [`gsb_game::aoi::AoiRoom`] with the given `cell_size` (world units per
/// cell edge). Group key is a spatial [`gsb_game::aoi::Cell`] — the
/// spatial path: one snapshot per cell, shared by reference with the
/// cell's occupants. Note the `RoomFactory`'s group-key associated type
/// differs from `open_room_factory`'s (`Cell` vs `()`), so the strategies
/// cannot be stored in one value — `start_inner` picks the factory at the
/// config boundary. This is entirely on the game/server side; `gsb-core`
/// stays generic over the group key and is untouched.
fn aoi_room_factory(
    cell_size: f32,
    spawn_half: f32,
    disconnect_grace: std::time::Duration,
) -> RoomFactory<World, gsb_game::aoi::Cell, (), ()> {
    Arc::new(move |_id, _config| BuiltRoom::Single {
        world: World::new(),
        logic: Box::new(
            gsb_game::aoi::AoiRoom::with_spawn_half(cell_size, spawn_half)
                .with_disconnect_grace(disconnect_grace),
        ) as Box<dyn RoomLogic<World, GroupKey = gsb_game::aoi::Cell, Strip = ()>>,
    })
}

/// The team-fog room factory: an empty bevy `World` + a
/// [`gsb_game::team::TeamRoom`] with the given `vision_radius`. Group key
/// is [`gsb_game::team::Team`] (2 groups).
fn team_room_factory(
    vision_radius: f32,
    spawn_half: f32,
    disconnect_grace: std::time::Duration,
) -> RoomFactory<World, gsb_game::team::Team, (), ()> {
    Arc::new(move |_id, _config| BuiltRoom::Single {
        world: World::new(),
        logic: Box::new(
            gsb_game::team::TeamRoom::with_spawn_half(vision_radius, spawn_half)
                .with_disconnect_grace(disconnect_grace),
        ) as Box<dyn RoomLogic<World, GroupKey = gsb_game::team::Team, Strip = ()>>,
    })
}

/// The PVS room factory: an empty bevy `World` + a
/// [`gsb_game::pvs::SectorRoom`] (the demo map is built into the room).
/// Group key is [`gsb_game::pvs::Sector`].
fn pvs_room_factory(
    spawn_half: f32,
    disconnect_grace: std::time::Duration,
) -> RoomFactory<World, gsb_game::pvs::Sector, (), ()> {
    Arc::new(move |_id, _config| BuiltRoom::Single {
        world: World::new(),
        logic: Box::new(
            gsb_game::pvs::SectorRoom::with_spawn_half(spawn_half)
                .with_disconnect_grace(disconnect_grace),
        ) as Box<dyn RoomLogic<World, GroupKey = gsb_game::pvs::Sector, Strip = ()>>,
    })
}

/// The sharded room factory: `shard_count` shards, each an empty bevy
/// [`World`] + a [`gsb_game::sharded::ShardedRoom`] over the same map
/// (half-size `spawn_half`). Unlike the other factories (which build one
/// `RoomLogic` room), this returns [`BuiltRoom::Sharded`]: N shard worlds
/// and N `ShardLogic` instances (the grid topology is the room's business,
/// the registry just wires the channels).
///
/// `economy` is the same ONE service-per-server the demo rooms share
/// (cloned into every shard — the Faz 3 promotion gives the sharded path
/// the RPC machinery, so its `ECONOMY` requests delegate like any other
/// room's).
///
/// `home_shard` routes a join to the shard owning the joiner's *spawn*
/// position (the deterministic `spawn_pos` → the grid region of that
/// point). The router is pure and synchronous (no await) — the registry
/// never blocks on it.
fn sharded_room_factory(
    spawn_half: f32,
    shard_count: usize,
    disconnect_grace: std::time::Duration,
    economy: gsb_game::economy::EconomyService,
) -> RoomFactory<World, (), gsb_game::sharded::ShardedRoomState, gsb_game::sharded::StripPos> {
    Arc::new(move |_id, _config| {
        let shards: Vec<
            gsb_core::registry::Shard<
                World,
                (),
                gsb_game::sharded::ShardedRoomState,
                gsb_game::sharded::StripPos,
            >,
        > =
            (0..shard_count)
                .map(|i| {
                    (
                        World::new(),
                        Box::new(
                            gsb_game::sharded::ShardedRoom::new(i, shard_count, spawn_half)
                                .with_disconnect_grace(disconnect_grace)
                                .with_economy(economy.clone()),
                        )
                            as Box<
                                dyn gsb_core::shard::ShardLogic<
                                    World,
                                    GroupKey = (),
                                    State = gsb_game::sharded::ShardedRoomState,
                                    Strip = gsb_game::sharded::StripPos,
                                >,
                            >,
                    )
                })
                .collect();
        BuiltRoom::Sharded {
            shards,
            home_shard: Arc::new(move |conn| {
                let (x, y) = gsb_game::room::spawn_pos(conn, spawn_half);
                gsb_game::sharded::shard_at(x, y, spawn_half, shard_count)
            }),
        }
    })
}

/// Start the server (local auth; no ticket hook). Must be called from
/// inside a tokio runtime. Metric reports go to the tracing logger (one
/// `gsb-metric` line per scope per second; visible under `RUST_LOG=info`,
/// silent without a subscriber).
pub async fn start_server(cfg: Config) -> Result<ServerHandle, ServerError> {
    start_server_with(cfg, ServerHooks::default()).await
}

/// Start the server (local auth; no ticket hook) with a programmatic
/// metrics consumer: each report is sent to `report_tx` (see
/// [`gsb_core::metrics`]). Used by the load generator and by tests that
/// assert on server-side counters.
pub async fn start_server_metrics(
    cfg: Config,
    report_tx: mpsc::UnboundedSender<MetricReport>,
) -> Result<ServerHandle, ServerError> {
    start_server_metrics_with(cfg, ServerHooks::default(), report_tx).await
}

/// Start the server with the platform's hooks (feature A, control-plane
/// entry): see [`ServerHooks`] for the ticket-validation hook. Everything
/// else is identical to [`start_server`].
pub async fn start_server_with(
    cfg: Config,
    hooks: ServerHooks,
) -> Result<ServerHandle, ServerError> {
    start_inner(cfg, MetricSink::Log, hooks).await
}

/// Start the server with the platform's hooks and a programmatic metrics
/// consumer (the [`start_server_with`] + [`start_server_metrics`]
/// composition; see both).
pub async fn start_server_metrics_with(
    cfg: Config,
    hooks: ServerHooks,
    report_tx: mpsc::UnboundedSender<MetricReport>,
) -> Result<ServerHandle, ServerError> {
    start_inner(cfg, MetricSink::Channel(report_tx), hooks).await
}

/// The config's grace as a `Duration`, clamped at zero: a negative value
/// would panic `from_secs_f64`, and "negative grace" can only mean
/// "disabled" anyway.
fn grace_of(cfg: &Config) -> std::time::Duration {
    std::time::Duration::from_secs_f64(cfg.disconnect_grace_secs.max(0.0))
}

/// Resolve the effective unauthenticated-connection cap ONCE, at startup
/// (see [`Config::max_unauth_conns`] for the semantics): an explicit
/// positive value wins; `Some(0)` disables the cap entirely; omission
/// derives `max(max_connections / 4, 64)` from the total cap — falling
/// back to [`DEFAULT_MAX_CONNECTIONS`] as the formula's base when the
/// total cap itself is unlimited (one derivation, one documented base).
fn unauth_cap_of(cfg: &Config) -> Option<u64> {
    match cfg.max_unauth_conns {
        Some(0) => None,
        Some(n) => Some(n),
        None => {
            let base = cfg.max_connections.unwrap_or(DEFAULT_MAX_CONNECTIONS);
            Some((base / 4).max(MIN_UNAUTH_CONNS))
        }
    }
}

/// The metrics report cadence. ONE constant for both sides of the
/// freshness contract: the collector emits every period, and the HTTP
/// `/healthz` threshold is "three periods since the last emission"
/// (`http.rs`) — deriving both from one value keeps the two honest if the
/// cadence ever changes.
const REPORT_PERIOD: std::time::Duration = std::time::Duration::from_secs(1);

async fn start_inner(
    cfg: Config,
    metric_sink: MetricSink,
    hooks: ServerHooks,
) -> Result<ServerHandle, ServerError> {
    // The listener table: reduce BOTH config grammars (the `[[listeners]]`
    // array or the derived-from-scalars single door) to validated specs —
    // parsed addresses, per-entry tls-file sanity, duplicate detection.
    // Checked BEFORE anything binds so a misconfigured server never
    // half-starts (the same principle the scalar era applied to its TLS
    // keys; see `resolve_listeners`).
    let specs = resolve_listeners(&cfg)?;

    // The three-axis selection (topology × visibility × communication):
    // derive the axes from the legacy spellings, honor explicit keys, and
    // validate the combination — BEFORE anything binds, so an unsupported
    // combination fails cleanly at startup naming its roadmap phase (the
    // same never-half-start principle as `resolve_listeners`).
    let selection = cfg.resolve_selection()?;

    // The sharded topology is a grid of 1..=256 shards (see
    // `gsb_game::sharded::grid_shape`); a count outside that range would
    // build a degenerate (or impossible) grid, so refuse to start. Gated
    // on the RESOLVED topology: both the legacy spelling AND an explicit
    // `topology = "sharded"` take this path.
    if selection.topology == Topology::Sharded && !(1..=256).contains(&cfg.shard_count) {
        return Err(ServerError::BadShardCount(cfg.shard_count));
    }

    let table = build_table();
    let (reg_tx, reg_rx) = channel::<RegistryMsg>(4096);

    // The HTTP ops surface (`docs/OPS.md`), enabled by a non-empty
    // `http_listen`. When enabled it BECOMES the metrics consumer: the
    // collector publishes each report into a `watch` channel (latest-wins
    // overwrite — a scraper between periods always sees the newest report,
    // and a slow reader can never back the collector up) instead of the
    // caller-provided sink, because `MetricSink` carries exactly one
    // destination. Documented consequence: combining
    // `start_server_metrics*` with `http_listen` redirects the reports to
    // the HTTP surface — a programmatic channel consumer requires leaving
    // `http_listen` empty. The watch's initial value is born five periods
    // stale, so `/healthz` answers 503 ("warming up") until the first real
    // report instead of ok from a placeholder nobody produced.
    let (metric_sink, http_task, http_addr) = if cfg.http_listen.is_empty() {
        (metric_sink, None, None)
    } else {
        let listen: SocketAddr = cfg.http_listen.parse().map_err(|e: std::net::AddrParseError| {
            ServerError::BadHttpListen(cfg.http_listen.clone(), e.to_string())
        })?;
        let listener = TcpListener::bind(listen)
            .await
            .map_err(|e| ServerError::BadHttpListen(cfg.http_listen.clone(), e.to_string()))?;
        let bound = listener.local_addr().map_err(|e| {
            ServerError::BadHttpListen(cfg.http_listen.clone(), e.to_string())
        })?;
        let (report_tx, report_rx) =
            watch::channel(MetricReport::initial_stale(REPORT_PERIOD));
        let task = http::spawn(
            listener,
            reg_tx.clone(),
            report_rx,
            REPORT_PERIOD,
            cfg.tick_hz,
            1..=cfg.room_count,
        );
        info!(addr = %bound, "http ops surface listening");
        (MetricSink::Watch(report_tx), Some(task), Some(bound))
    };

    // The global ticker: one broadcast channel + one timing task. Rooms
    // subscribe to it at creation; aborting the task closes the broadcast,
    // which is the rooms' global stop signal (in addition to the control
    // Shutdown they receive during registry teardown). The metrics
    // collector subscribes to the same broadcast as its clock (see
    // `gsb_core::metrics` for the design). A config rate without a period
    // is a startup error, not a runtime condition: fail here with a typed
    // error instead of letting the ticker panic mid-startup.
    let (ticker, ticker_task) = gsb_core::ticker::Ticker::spawn(cfg.tick_hz, 64)
        .map_err(|_| ServerError::BadTickRate(cfg.tick_hz))?;
    // A3: the metrics event channel is *bounded* (DESIGN §2: bounded capacity
    // is the backpressure mechanism) and producers send with the synchronous
    // `try_send` (a drop is counted, harmless — the counters are cumulative).
    // Capacity 4096 ≈ the worst burst: N startup `ConnOpened` registry samples
    // + N shutdown final-flush connection samples (2N ≈ 2000 at 1000 conns),
    // with ~60× headroom over the collector's steady-state occupancy (it drains
    // the whole channel every tick; a tick holds only ~tens of samples). A drop
    // could still happen under a pathological stall — it is counted and
    // reported, never a stall.
    let (metrics_tx, metrics_rx) = mpsc::channel::<MetricsEvent>(4096);
    let metrics = tokio::spawn(
        MetricsCollector::new(
            ticker.subscribe(),
            metrics_rx,
            metric_sink,
            REPORT_PERIOD,
        )
        .run(),
    );

    // The match-result sink (the control plane's result seam, feature A):
    // a bounded mailbox the composition root reads from via
    // `ServerHandle::match_results` (the reference adapter — in-process,
    // one hop; the base ships no NATS/Kafka/gRPC). Cloned to each room at
    // creation; a room without a configured result reports nothing.
    let (result_tx, result_rx) = channel::<MatchResult>(64);

    // The registry runs until Shutdown; dropping the handle is fine. It
    // keeps a clone of its own mailbox so dispatcher tasks can report back.
    // The factory (and hence the registry's group-key type) is chosen from
    // the RESOLVED three-axis selection — never the raw legacy string: each
    // room kind is a different `RoomLogic` group key (`()`, `Cell`, `Team`,
    // `Sector`) or the sharded grid topology, so the arms are otherwise
    // identical and each yields a `JoinHandle<()>`. `resolve_selection`
    // already validated the combination; every arm here is a supported one.
    let _registry = match selection.kind {
        RoomKind::Open => {
            // One economy service per server (the RPC pattern's
            // external-I/O reference adapter; shared by clone with every
            // demo room the factory builds).
            let economy = gsb_game::economy::EconomyService::spawn(
                gsb_game::economy::EconomyService::default_latency(),
            );
            let disconnect_grace = grace_of(&cfg);
            tokio::spawn(
                Registry::new(
                    reg_rx,
                    reg_tx.clone(),
                    open_room_factory(cfg.spawn_half_size, disconnect_grace, economy),
                    ticker.clone(),
                    metrics_tx.clone(),
                    cfg.max_connections,
                    unauth_cap_of(&cfg),
                    Some(result_tx.clone()),
                )
                .run(),
            )
        }
        RoomKind::Aoi => {
            let disconnect_grace = grace_of(&cfg);
            tokio::spawn(
                Registry::new(
                    reg_rx,
                    reg_tx.clone(),
                    aoi_room_factory(cfg.aoi_cell_size, cfg.spawn_half_size, disconnect_grace),
                    ticker.clone(),
                    metrics_tx.clone(),
                    cfg.max_connections,
                    unauth_cap_of(&cfg),
                    Some(result_tx.clone()),
                )
                .run(),
            )
        }
        RoomKind::Team => {
            let disconnect_grace = grace_of(&cfg);
            tokio::spawn(
                Registry::new(
                    reg_rx,
                    reg_tx.clone(),
                    team_room_factory(cfg.team_vision_radius, cfg.spawn_half_size, disconnect_grace),
                    ticker.clone(),
                    metrics_tx.clone(),
                    cfg.max_connections,
                    unauth_cap_of(&cfg),
                    Some(result_tx.clone()),
                )
                .run(),
            )
        }
        RoomKind::Sector => {
            let disconnect_grace = grace_of(&cfg);
            tokio::spawn(
                Registry::new(
                    reg_rx,
                    reg_tx.clone(),
                    pvs_room_factory(cfg.spawn_half_size, disconnect_grace),
                    ticker.clone(),
                    metrics_tx.clone(),
                    cfg.max_connections,
                    unauth_cap_of(&cfg),
                    Some(result_tx.clone()),
                )
                .run(),
            )
        }
        RoomKind::Sharded => {
            let disconnect_grace = grace_of(&cfg);
            // One economy service per server, shared with the shards (the
            // Faz 3 promotion: the sharded path runs the full RPC
            // machinery, so `ECONOMY` requests delegate exactly like the
            // single-room demo's).
            let economy = gsb_game::economy::EconomyService::spawn(
                gsb_game::economy::EconomyService::default_latency(),
            );
            tokio::spawn(
                Registry::new(
                    reg_rx,
                    reg_tx.clone(),
                    sharded_room_factory(
                        cfg.spawn_half_size,
                        cfg.shard_count as usize,
                        disconnect_grace,
                        economy,
                    ),
                    ticker.clone(),
                    metrics_tx.clone(),
                    cfg.max_connections,
                    unauth_cap_of(&cfg),
                    // Faz 3: every shard reports ITS final state through
                    // the shared sink at its own teardown — one payload
                    // per shard under the logical room id (the adapter
                    // concatenates/filters; see `gsb_core::shard`).
                    Some(result_tx.clone()),
                )
                .run(),
            )
        }
    };

    // Pre-create rooms 1..=room_count (all at the global rate; a room may
    // configure a slower rate that divides it).
    for id in 1..=cfg.room_count {
        let config = RoomConfig {
            id: RoomId(id),
            tick_hz: cfg.tick_hz,
            control_capacity: cfg.room_control,
            action_capacity: cfg.conn_action,
            max_snapshot_bytes: cfg.max_snapshot_bytes,
            keepalive_hz: cfg.keepalive_hz,
            max_players: cfg.max_players.map(|n| n as usize),
            ..Default::default()
        };
        {
            let tx = reg_tx.clone();
            tokio::spawn(async move {
                let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
                if tx
                    .send(RegistryMsg::CreateRoom {
                        config,
                        reply: reply_tx,
                    })
                    .await
                    .is_err()
                {
                    return;
                }
                match reply_rx.await {
                    Ok(Ok(status)) => info!(?status, "room created"),
                    Ok(Err(e)) => warn!(error = %e, "room creation failed"),
                    Err(_) => warn!("registry gone before room reply"),
                }
            });
        }
    }

    // Session-lifecycle idle window (`0` disables): the reader pump's
    // clock on TCP, the demux deadline heap's window on rUDP.
    let idle_timeout = (cfg.idle_timeout_secs > 0.0).then(|| {
        std::time::Duration::from_secs_f64(cfg.idle_timeout_secs)
    });

    // The rUDP cookie key: the operator's 32-hex-char config string, or
    // `None` = the transport draws 16 bytes from the OS entropy source
    // at bind time. A parse failure is a config error (the operator sees
    // it at startup, before anything binds); an entropy failure is a
    // bind error (the server refuses to start with a predictable key —
    // see `gsb_net::udp::CookieKey`). It stays a GLOBAL knob: every rUDP
    // listener draws its own socket (and its own demux), but they all run
    // the same handshake policy — per-listener keys would let an operator
    // quietly weaken one door of an otherwise identical deployment.
    let has_udp = specs
        .iter()
        .any(|s| matches!(s, ListenerSpec::Udp { .. }));
    let cookie_key = if has_udp {
        cfg.udp_cookie_key
            .as_deref()
            .map(parse_cookie_key)
            .transpose()
            .map_err(ServerError::BadCookieKey)?
    } else {
        None
    };

    // Bind EVERY listener before spawning any accept task. The TLS pick
    // rides the Tcp-shaped spec: TLS is a socket-level upgrade of the SAME
    // framing, so the accept loops, pumps and every actor below are
    // identical for plaintext and encrypted doors (see `gsb_net::tls`). The
    // PEM files are loaded inside `bind`; a missing/malformed file surfaces
    // here as a bind error with the path named. On a partial failure the
    // already-bound listeners are closed explicitly (not just dropped): a
    // dropped `UdpListener` would leave its demux task reading the socket —
    // `Listener::close` is the only door that stops it.
    let mut listeners: Vec<Arc<dyn gsb_net::transport::Listener>> =
        Vec::with_capacity(specs.len());
    let mut addrs: Vec<SocketAddr> = Vec::with_capacity(specs.len());
    for spec in &specs {
        match bind_listener(spec, &cfg, idle_timeout, cookie_key).await {
            Ok((listener, addr)) => {
                listeners.push(listener);
                addrs.push(addr);
            }
            Err(e) => {
                for l in &listeners {
                    l.close();
                }
                return Err(e);
            }
        }
    }

    // ONE shared connection-id sequence for ALL accept tasks (see
    // `ConnIdSeq`), cloned into each loop with the rest of the pipeline.
    let pipeline = AcceptPipeline {
        registry: reg_tx.clone(),
        metrics: metrics_tx,
        table,
        ticket_auth: hooks.ticket,
        conn_inbox: cfg.conn_inbox,
        conn_out: cfg.conn_out,
        idle_timeout,
        conn_ids: Arc::new(ConnIdSeq::new()),
    };

    // One accept task PER listener; every accepted endpoint flows through
    // the SAME pipeline (same registry, same rooms, same id sequence), so
    // rooms never learn which door a client came in through.
    let mut accepts = Vec::with_capacity(listeners.len());
    for (i, listener) in listeners.iter().enumerate() {
        let addr = addrs[i];
        accepts.push(tokio::spawn(run_accept(
            pipeline.clone(),
            Arc::clone(listener),
            addr,
        )));
    }

    Ok(ServerHandle {
        registry: reg_tx,
        accepts,
        ticker: ticker_task,
        metrics,
        http: http_task,
        listeners,
        addr: addrs[0],
        addrs,
        http_addr,
        match_results: result_rx,
    })
}

/// The server-wide connection-id sequence: ONE monotonic counter shared by
/// every accept task.
///
/// WHY a central counter instead of per-listener ranges or registry-assigned
/// ids: the registry (and every room) keys connections by [`ConnectionId`],
/// so a collision across two doors would silently re-route one client's
/// frames into another's inbox — uniqueness is a correctness invariant, not
/// a naming nicety. A single atomic fetch_add gives it with zero contention
/// concerns (one relaxed RMW per connection birth; accepts are human-scale
/// events even at load) and keeps ids dense from 1 like the single-loop era
/// did (`c1`, `c2`, … in logs/metrics stay interpretable). Registry-minted
/// ids were rejected because they would put a control-plane round trip on
/// the accept hot path and couple transport intake to registry liveness;
/// per-listener ranges were rejected because they leak listener identity
/// into the id space and complicate the cap accounting for no benefit.
struct ConnIdSeq {
    /// The LAST minted raw value (0 = nothing minted yet, so the first
    /// connection gets `ConnectionId(1)` exactly as the old per-loop
    /// counter did).
    last: AtomicU64,
}

impl ConnIdSeq {
    /// A fresh sequence starting below the first connection id.
    fn new() -> Self {
        Self {
            last: AtomicU64::new(0),
        }
    }

    /// Mint the next unique id. `Relaxed`: the counter synchronizes nothing
    /// but its own monotonicity — no other memory is published through it —
    /// and atomics never go backwards, so distinct mints are distinct ids.
    fn mint(&self) -> ConnectionId {
        ConnectionId(self.last.fetch_add(1, Ordering::Relaxed) + 1)
    }
}

/// Everything ONE accept loop needs to push an accepted endpoint through
/// the shared pipeline: the control plane, the metrics producer, the wire
/// table, the auth hook and the channel-capacity policy. Cloned per
/// listener (senders and `Arc`s — cheap); immutable after construction, so
/// sharing needs no synchronization beyond the clone itself.
#[derive(Clone)]
struct AcceptPipeline {
    /// Where `ConnOpened` goes (cloned again per spawned actor).
    registry: Mailbox<RegistryMsg>,
    /// Metrics producer handle for connection actors.
    metrics: mpsc::Sender<MetricsEvent>,
    /// The decoded-frame dispatch table (base + demo game ops).
    table: Arc<MessageTable>,
    /// Ticket-validation hook (`None` = local auth), cloned per connection.
    ticket_auth: Option<TicketAuth>,
    /// Inbound mailbox capacity (endpoint fallback + actor construction).
    conn_inbox: usize,
    /// Outbound channel capacity (same contract as `conn_inbox`).
    conn_out: usize,
    /// The session idle window (`None` disables) handed to the pumps.
    idle_timeout: Option<std::time::Duration>,
    /// THE shared id sequence across every listener's loop.
    conn_ids: Arc<ConnIdSeq>,
}

/// One listener's accept loop: take the next endpoint, mint a globally
/// unique connection id, then hand the endpoint to the ordinary pipeline
/// (pumps → `ConnOpened` → connection actor) — byte-for-byte the flow the
/// single-listener era ran, just entered from N doors. Runs until the task
/// is aborted by `ServerHandle::stop` (after `Listener::close` made
/// `accept` fail fast on transports with shared state).
async fn run_accept(
    pipeline: AcceptPipeline,
    listener: Arc<dyn gsb_net::transport::Listener>,
    addr: SocketAddr,
) {
    info!(%addr, "accepting connections");
    loop {
        // `accept` consumes the Arc; clone it per iteration.
        let l = Arc::clone(&listener);
        let mut endpoint = match l.accept().await {
            Ok(endpoint) => endpoint,
            Err(e) => {
                warn!(%e, "accept error; backing off");
                // Back off: a persistent error (e.g. EMFILE) must not
                // turn this loop into a CPU-burning spin.
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                continue;
            }
        };

        let conn = pipeline.conn_ids.mint();

        // The peer address (for the connection actor's violation-close
        // signal — see `gsb_core::conn`); a transport that does not
        // expose one reports an unspecified address.
        let peer = endpoint
            .peer()
            .unwrap_or_else(|| std::net::SocketAddr::from(([0, 0, 0, 0], 0)));

        // The connection's mailboxes, from the endpoint: pre-created
        // by a transport that establishes the session itself (rUDP:
        // at handshake, before this loop runs), created here for a
        // transport that does not (TCP — the exact channels this loop
        // used to create directly).
        let (in_tx, in_rx) = endpoint.take_inbox(pipeline.conn_inbox);
        let (out_tx, out_rx) = endpoint.take_outbox(pipeline.conn_out);

        // Reader + writer pumps (they finish on their own when the
        // peer or the actor goes away; the idle window, if enabled,
        // is what detects a half-open peer that sends nothing).
        let _pumps = endpoint.start_pump(conn, in_tx.clone(), out_rx, pipeline.idle_timeout);

        // Register before spawning the actor: the registry owns the
        // notification path, and it must know the inbox before any
        // frame can reach the actor.
        let _ = pipeline
            .registry
            .send(RegistryMsg::ConnOpened {
                conn,
                inbox: in_tx.clone(),
            })
            .await;

        // One cheap sender clone per connection (unbounded sender is
        // an Arc).
        tokio::spawn(
            ConnectionActor::new(
                conn,
                peer,
                Arc::clone(&pipeline.table),
                pipeline.registry.clone(),
                in_rx,
                out_tx,
                pipeline.metrics.clone(),
                pipeline.ticket_auth.clone(),
            )
            .run(),
        );
    }
}

/// Build and bind ONE listener from a validated spec. The transport
/// instance is per-listener ON PURPOSE even for two entries of the same
/// kind: each door owns its socket (and, for rUDP, its own demux state),
/// so closing one listener can never disturb another's sessions.
async fn bind_listener(
    spec: &ListenerSpec,
    cfg: &Config,
    idle_timeout: Option<std::time::Duration>,
    cookie_key: Option<[u8; 16]>,
) -> Result<(Arc<dyn gsb_net::transport::Listener>, SocketAddr), ServerError> {
    let transport: Arc<dyn Transport> = match spec {
        ListenerSpec::Tcp { .. } => Arc::new(TcpTransport {
            max_frame_bytes: cfg.max_frame_bytes,
        }),
        ListenerSpec::Tls { cert_pem, key_pem, .. } => Arc::new(TlsTransport {
            config: TlsTransportConfig {
                cert_chain_pem: cert_pem.clone(),
                key_pem: key_pem.clone(),
                max_frame_bytes: cfg.max_frame_bytes,
            },
        }),
        ListenerSpec::Udp { .. } => Arc::new(UdpTransport {
            config: UdpTransportConfig {
                // The demux pre-creates the mailboxes at handshake: same
                // capacities as the TCP path (cfg.conn_inbox/conn_out are
                // the fallbacks `Endpoint::take_*` would use).
                inbox_capacity: cfg.conn_inbox,
                outbox_capacity: cfg.conn_out,
                max_datagram_bytes: cfg.udp_max_datagram_bytes,
                idle_timeout,
                cookie_key,
            },
        }),
    };
    let listener = transport.bind(spec.addr()).await?;
    let addr = listener.local_addr().ok_or_else(|| {
        ServerError::BadBind(
            spec.addr().to_string(),
            "listener reports no address".into(),
        )
    })?;
    Ok((listener, addr))
}
