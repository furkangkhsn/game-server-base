//! Errors the configuration can produce, and the axis resolution
//! itself lives with the Config impl in the parent.

use std::net::SocketAddr;

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

    #[error(
        "topology = \"sharded\" with visibility = \"{0}\" breaks shard \
             locality: cross-shard interest needs a subscription layer \
             that does not exist yet (docs/CROSS-SHARD.md §4 keeps every \
             interaction design shard-local); run team/pvs rooms on \
             topology = \"single\""
    )]
    ShardedCrossInterest(String),

    #[error(
        "communication = \"delta\" with topology = \"single\" needs a \
             visibility that serves delta today: only spatial (AoiRoom) \
             does; all/team/pvs have no delta packaging yet (ROADMAP: \
             ortak DeltaSnapshotCodec) — use visibility = \"spatial\" or \
             communication = \"always-full\""
    )]
    SingleDelta,

    #[error(
        "communication = \"delta\" with topology = \"sharded\" needs a \
             visibility that serves delta today: only spatial does (the \
             sharded × spatial composite of ROADMAP Faz B); all/team/pvs \
             have no delta packaging on the grid, and per-link \
             communication derivation arrives with ROADMAP Faz C — use \
             visibility = \"spatial\" or communication = \"always-full\""
    )]
    ShardedDelta,

    #[error(
        "communication = \"always-full\" with visibility = \"spatial\" \
             asks for packaging no spatial room serves: AoiRoom encodes \
             per-cell DELTA pieces and has no full-frame mode (its fulls \
             are the keep-alive/late-join recovery path, not a wire \
             setting) — omit `communication` (it derives delta) or move to \
             visibility = \"all\"/\"team\"/\"pvs\""
    )]
    SingleAlwaysFull,

    #[error(
        "communication = \"always-full\" with visibility = \"spatial\" \
             asks for packaging no spatial room serves: the sharded × \
             spatial composite (ROADMAP Faz B) broadcasts per-shard \
             cell-grouped DELTA and has no full-frame mode — omit \
             `communication` (it derives delta), or use \
             visibility = \"all\" for whole-world frames per shard"
    )]
    ShardedAlwaysFull,

    #[error(
        "invalid `tick_hz` {0}: must be finite and > 0 (the global ticker derives its period as 1/hz; a rate without a period refuses startup instead of panicking)"
    )]
    BadTickRate(f64),

    #[error("invalid `http_listen` address `{0}`: {1}")]
    BadHttpListen(String, String),

    #[error(
        "`tls_cert` is set but `tls_key` is empty: TLS needs BOTH files; \
             refusing to start half-configured (a silent plaintext fallback \
             would hide the mistake) — docs/SECURITY.md §2 decision 3"
    )]
    TlsCertNeedsKey,

    #[error(
        "`tls_key` is set but `tls_cert` is empty: TLS needs BOTH files; \
             refusing to start half-configured — docs/SECURITY.md §2 decision 3"
    )]
    TlsKeyNeedsCert,

    #[error(
        "transport = \"udp\" cannot be combined with tls_cert/tls_key: \
             rUDP is experimental and takes no TLS (its cookie handshake is \
             its own anti-amplification boundary) — docs/SECURITY.md §2 \
             decision 7"
    )]
    UdpWithTls,

    #[error(
        "`[[listeners]]` is present but empty: a server with no listener \
             cannot accept clients; remove the empty table to fall back to \
             the legacy scalar keys, or add entries — never an implicit \
             fallback that hides a half-edited config"
    )]
    EmptyListeners,

    #[error(
        "duplicate bind address `{0}` in `[[listeners]]`: two listeners \
             cannot own one address (the second bind would fail anyway; \
             reporting it at config time names the culprit instead of \
             failing inside a bind syscall)"
    )]
    DuplicateBind(String),

    #[error(
        "`[[listeners]]` entry `{bind}` sets transport = \"tls\" with \
             tls_cert but no tls_key: TLS needs BOTH files; refusing to \
             start half-configured — docs/SECURITY.md §2 decision 3"
    )]
    ListenerTlsCertNeedsKey {
        /// The offending entry's bind address (names the entry in logs).
        bind: String,
    },

    #[error(
        "`[[listeners]]` entry `{bind}` sets transport = \"tls\" with \
             tls_key but no tls_cert: TLS needs BOTH files; refusing to \
             start half-configured — docs/SECURITY.md §2 decision 3"
    )]
    ListenerTlsKeyNeedsCert {
        /// The offending entry's bind address (names the entry in logs).
        bind: String,
    },

    #[error(
        "`[[listeners]]` entry `{bind}`: transport = \"tcp\" cannot carry \
             tls_cert/tls_key — write transport = \"tls\" for an encrypted \
             door (a plaintext door with TLS files attached is a config \
             mistake, not something to silently reinterpret)"
    )]
    ListenerTcpWithTls {
        /// The offending entry's bind address (names the entry in logs).
        bind: String,
    },

    #[error(
        "`[[listeners]]` entry `{bind}`: transport = \"udp\" cannot be \
             combined with tls_cert/tls_key: rUDP is experimental and takes \
             no TLS — docs/SECURITY.md §2 decision 7"
    )]
    ListenerUdpWithTls {
        /// The offending entry's bind address (names the entry in logs).
        bind: String,
    },

    #[error(
        "`[[listeners]]` entry `{bind}` sets transport = \"quic\" with \
             tls_cert but no tls_key: QUIC is TLS 1.3 underneath and needs \
             BOTH files; refusing to start half-configured (the same rule \
             as the \"tls\" door)"
    )]
    ListenerQuicCertNeedsKey {
        /// The offending entry's bind address (names the entry in logs).
        bind: String,
    },

    #[error(
        "`[[listeners]]` entry `{bind}` sets transport = \"quic\" with \
             tls_key but no tls_cert: QUIC is TLS 1.3 underneath and needs \
             BOTH files; refusing to start half-configured (the same rule \
             as the \"tls\" door)"
    )]
    ListenerQuicKeyNeedsCert {
        /// The offending entry's bind address (names the entry in logs).
        bind: String,
    },

    #[error(
        "`[[listeners]]` entry `{bind}`: transport = \"ws\" cannot carry \
             tls_cert/tls_key — the WebSocket door upgrades PLAIN TCP today \
             (a wss:// door would be ws-over-TLS-TCP, a composition this \
             grammar does not express yet); attach the files to a \"tls\" \
             entry instead of silently reinterpreting this one"
    )]
    ListenerWsWithTls {
        /// The offending entry's bind address (names the entry in logs).
        bind: String,
    },

    #[error(
        "`[rooms.{id}]` builds a room the registry would refuse: {source} \
             (a room's tick_hz must divide the global tick_hz, and its \
             keepalive_hz must not exceed its tick_hz)"
    )]
    RoomOverride {
        /// The overridden room's id.
        id: u64,
        /// The registry's rule the resulting room breaks.
        #[source]
        source: gsb_core::error::CoreError,
    },

    /// Game selection or a game module's own configuration failed (see
    /// [`crate::GameError`]).
    #[error(transparent)]
    Game(#[from] crate::GameError),
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
    /// QUIC (quinn over one UDP socket) with these PEM files (loaded at
    /// bind time; the SAME pair a "tls" door would load).
    Quic {
        addr: SocketAddr,
        cert_pem: String,
        key_pem: String,
    },
    /// WebSocket upgrade over plain TCP (`gsb_net::ws`); no per-entry
    /// files (wss is not a spelling this grammar expresses — see
    /// [`ServerError::ListenerWsWithTls`]).
    Ws { addr: SocketAddr },
}
