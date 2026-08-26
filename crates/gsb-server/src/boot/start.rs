//! Startup: bind the doors, install the rooms, start the ticker and
//! the collector, and hand back a handle that can stop all of it.

use std::net::SocketAddr;
use std::sync::Arc;

use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio::sync::watch;
use tracing::{info, warn};


use gsb_core::channel::channel;
use gsb_core::id::RoomId;
use gsb_core::metrics::{MetricReport, MetricSink, MetricsCollector, MetricsEvent};
use gsb_core::registry::{MatchResult, Registry, RegistryMsg};
use gsb_core::room::RoomConfig;
use crate::boot::accept::*;
use crate::boot::factories::*;
use crate::config::*;
use crate::*;

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
        RoomKind::ShardedSpatial => {
            let disconnect_grace = grace_of(&cfg);
            // One economy service per server, shared with the shards —
            // identical wiring to the whole-shard grid above.
            let economy = gsb_game::economy::EconomyService::spawn(
                gsb_game::economy::EconomyService::default_latency(),
            );
            tokio::spawn(
                Registry::new(
                    reg_rx,
                    reg_tx.clone(),
                    sharded_spatial_room_factory(
                        cfg.spawn_half_size,
                        cfg.shard_count as usize,
                        cfg.aoi_cell_size,
                        disconnect_grace,
                        economy,
                    ),
                    ticker.clone(),
                    metrics_tx.clone(),
                    cfg.max_connections,
                    unauth_cap_of(&cfg),
                    // Same per-shard result reporting as the plain grid.
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
