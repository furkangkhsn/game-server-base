//! Startup: bind the doors, install the rooms, start the ticker and
//! the collector, and hand back a handle that can stop all of it.

use std::net::SocketAddr;
use std::sync::Arc;

use tokio::sync::mpsc;
use tokio::sync::watch;
use tracing::info;

use crate::boot::accept::*;
use crate::config::*;
use crate::*;
use gsb_core::channel::channel;
use gsb_core::metrics::{MetricReport, MetricSink, MetricsCollector, MetricsEvent};
use gsb_core::registry::{MatchResult, RegistryMsg};

mod boot_rooms;
mod entry;
pub use entry::*;
mod export;
pub(super) mod ops_door;
mod pre_auth;
use pre_auth::{handshake_bound_of, unauth_cap_of};

/// The metrics report cadence. ONE constant for both sides of the
/// freshness contract: the collector emits every period, and the HTTP
/// `/healthz` threshold is "three periods since the last emission"
/// (`http.rs`) — deriving both from one value keeps the two honest if the
/// cadence ever changes.
const REPORT_PERIOD: std::time::Duration = std::time::Duration::from_secs(1);

/// Start `module` under `cfg`: the one startup procedure every public
/// entry point funnels into.
async fn start_inner(
    mut module: Box<dyn GameModule>,
    cfg: Config,
    metric_sink: MetricSink,
    hooks: ServerHooks,
) -> Result<ServerHandle, ServerError> {
    // The top level of the file (BACKLOG F62): a key neither the engine
    // nor a game compiled into this build owns refuses startup, first.
    cfg.check_top_level_keys(&*module)?;

    // The listener table: reduce BOTH config grammars (the `[[listeners]]`
    // array or the derived-from-scalars single door) to validated specs —
    // parsed addresses, per-entry tls-file sanity, duplicate detection.
    // Checked BEFORE anything binds so a misconfigured server never
    // half-starts (the same principle the scalar era applied to its TLS
    // keys; see `resolve_listeners`).
    let specs = resolve_listeners(&cfg)?;
    // The accept backlog every TCP-based socket gets (B84): refused here,
    // before the first bind, like every other listener key.
    check_listen_backlog(&cfg)?;
    // The socket buffers every UDP-based door asks for (B4), the same way.
    check_udp_buffers(&cfg)?;

    // The room-level keys (flat and `[rooms.<id>]`): a value no room can
    // run with refuses startup; so does a room the registry would refuse
    // (at boot it would only warn).
    cfg.check_room_keys()?;
    cfg.check_room_overrides()?;

    // The push exporters (`[metrics]`): a table this build has no
    // exporter for, or a target the exporter refuses, refuses startup.
    let exporters = export::exporters(&cfg)?;

    // The game's own settings (the demo: its three-axis selection and
    // shard-count check), validated BEFORE anything binds, so a bad game
    // config fails cleanly at startup (the same never-half-start
    // principle as `resolve_listeners`).
    module.configure(&cfg.raw, &cfg)?;
    info!(game = module.name(), selection = %module.describe(), "game module configured");

    // The rooms this server builds — boot, admin open, `room_config` —
    // from ONE template: the config's room-level keys over the game's
    // room defaults, its input rate limit and its idle ceiling's action
    // (read after `configure`: they may depend on the game's settings).
    let template = cfg.room_template_for(crate::config::GameDefaults {
        input_rate: module.input_rate(),
        afk_action: module.afk_action(),
    });

    // The wire table: the base protocol plus the game's messages.
    let table = {
        let mut table = gsb_protocol::base_table();
        module.register(&mut table);
        Arc::new(table)
    };
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
    // The transport tasks' metrics channel (F35; see the collector
    // below), made here: the ops surface reports on it too (B49).
    let (transport_tx, transport_rx) = mpsc::channel::<MetricsEvent>(4096);
    let (metric_sink, http_task, http_addr) = if cfg.http_listen.is_empty() {
        (metric_sink, None, None)
    } else {
        let (listener, bound) = ops_door::bind_ops(&cfg)?;
        let (report_tx, report_rx) = watch::channel(MetricReport::initial_stale(REPORT_PERIOD));
        let surface = http::spawn(
            listener,
            reg_tx.clone(),
            report_rx,
            REPORT_PERIOD,
            template.clone(),
            1..=cfg.room_count,
            http::OpsGuard {
                limits: http::OpsLimits::of(&cfg),
                metrics: Some(transport_tx.clone()),
            },
        );
        info!(addr = %bound, "http ops surface listening");
        (MetricSink::Watch(report_tx), Some(surface), Some(bound))
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
    //
    // Two channels, one collector (BACKLOG F35): the SESSION producers —
    // the registry, its rooms, shards and dispatchers, the connection
    // actors — send on `metrics_tx`, and the collector's final report
    // waits (bounded) until every one of them has dropped its sender, so
    // it carries every room's and connection's last word. The transport
    // tasks (the doors' pumps, the rUDP demux and writers, the handshake
    // intakes, the ops surface's accept loop) send on `transport_tx`
    // (made above): folded the same way, but not
    // waited for — a pump ends with its socket, which a silent peer can
    // hold open past the stop. Same capacity, same drop accounting.
    let (metrics_tx, metrics_rx) = mpsc::channel::<MetricsEvent>(4096);
    let metrics = tokio::spawn(
        MetricsCollector::new(ticker.subscribe(), metrics_rx, metric_sink, REPORT_PERIOD)
            .with_transport_events(transport_rx)
            .with_exporters(exporters)
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
    // The game module picks the room factory (and with it the registry's
    // generic types) and spawns the registry through `RegistryParts`.
    //
    // The rooms' drop barrier (BACKLOG F5): the registry and every room's
    // death watcher hold a token; `stop` waits on the other half before
    // it stops the game's services.
    let (rooms_hold, rooms_released) = gsb_core::service::hold();
    let registry = module.spawn_registry(RegistryParts {
        inbox: reg_rx,
        self_mailbox: reg_tx.clone(),
        ticker: ticker.clone(),
        metrics: metrics_tx.clone(),
        max_connections: cfg.max_connections,
        max_unauth_conns: unauth_cap_of(&cfg),
        result_sink: Some(result_tx.clone()),
        rooms_hold,
        services: Vec::new(),
    });

    // Pre-create rooms 1..=room_count (at the global rate, unless the
    // id's `[rooms.<id>]` sets a slower rate that divides it). Enqueued
    // HERE, in id order, before any accept loop exists (B44): the
    // registry drains its mailbox in order, so every join is behind
    // them. Only the replies are awaited, from a spawned task.
    boot_rooms::create_boot_rooms(&reg_tx, &template, cfg.room_count);

    // The session-lifecycle pair, one per socket direction (`0` disables
    // either): the reader pump's idle window — the demux deadline heap's
    // window on rUDP — and the writer pump's write stall.
    let idle_timeout = (cfg.idle_timeout_secs > 0.0)
        .then(|| std::time::Duration::from_secs_f64(cfg.idle_timeout_secs));
    let timeouts = gsb_net::pump::PumpTimeouts {
        idle: idle_timeout,
        write_stall: (cfg.write_stall_secs > 0.0)
            .then(|| std::time::Duration::from_secs_f64(cfg.write_stall_secs)),
    };

    // The rUDP cookie key: the operator's 32-hex-char config string, or
    // `None` = the transport draws 16 bytes from the OS entropy source
    // at bind time. A parse failure is a config error (the operator sees
    // it at startup, before anything binds); an entropy failure is a
    // bind error (the server refuses to start with a predictable key —
    // see `gsb_net::udp::CookieKey`). It stays a GLOBAL knob: every rUDP
    // listener draws its own socket (and its own demux), but they all run
    // the same handshake policy — per-listener keys would let an operator
    // quietly weaken one door of an otherwise identical deployment.
    let has_udp = specs.iter().any(|s| matches!(s, ListenerSpec::Udp { .. }));
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
    let handshake_bound = handshake_bound_of(&cfg);
    let mut listeners: Vec<Arc<dyn gsb_net::transport::Listener>> = Vec::with_capacity(specs.len());
    let mut addrs: Vec<SocketAddr> = Vec::with_capacity(specs.len());
    for spec in &specs {
        // The transport's own losses go to the same collector (B58), on
        // the channel its final report does not wait for (F35).
        let metrics = Some(transport_tx.clone());
        match bind_listener(
            spec,
            &cfg,
            idle_timeout,
            cookie_key,
            handshake_bound,
            metrics,
        )
        .await
        {
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
        timeouts,
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
        rooms: template,
        accepts,
        ticker: ticker_task,
        metrics,
        services: registry.services,
        rooms_released,
        http: http_task,
        listeners,
        addr: addrs[0],
        addrs,
        http_addr,
        match_results: result_rx,
    })
}
