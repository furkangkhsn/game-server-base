//! The in-process server the default mode runs against: a real
//! server on an ephemeral port, its metrics captured from the channel.


use gsb_core::metrics::MetricReport;

use tokio::sync::mpsc;

pub(crate) struct InProcessServer {
    pub(crate) handle: gsb_server::ServerHandle,
    pub(crate) rep_rx: mpsc::UnboundedReceiver<MetricReport>,
}

/// Capacity / lifecycle probe overrides for the in-process / served
/// server. Each `None` = unspecified (keep the server config default);
/// an explicit `0` = unlimited (rooms) / disabled (idle window).
pub(crate) struct ServerOverrides {
    pub(crate) max_players: Option<u32>,
    pub(crate) max_connections: Option<u64>,
    pub(crate) idle_timeout_secs: Option<f64>,
    /// The demo rooms' disconnect-park grace (`None` = config default).
    pub(crate) disconnect_grace_secs: Option<f64>,
}

pub(crate) fn apply_overrides(cfg: &mut gsb_server::Config, o: &ServerOverrides) {
    if let Some(n) = o.max_players {
        cfg.max_players = (n != 0).then_some(n);
    }
    if let Some(n) = o.max_connections {
        cfg.max_connections = (n != 0).then_some(n);
    }
    if let Some(s) = o.idle_timeout_secs {
        cfg.idle_timeout_secs = s;
    }
    if let Some(s) = o.disconnect_grace_secs {
        cfg.disconnect_grace_secs = s.max(0.0);
    }
}

/// Start the server in-process with a channel metrics sink. The receiver
/// moves into the report-drain task; nothing is shared across tasks
/// beyond that mailbox. `visibility` selects the room strategy (the five:
/// `()` / `Cell` / `Team` / `Sector` / sharded-grid).
#[allow(clippy::too_many_arguments)] // loadgen helper; params are natural
pub(crate) async fn start_inprocess(
    visibility: gsb_server::Visibility,
    topology: Option<gsb_server::Topology>,
    shard_count: u32,
    cell_size: f32,
    vision_radius: f32,
    max_snapshot_bytes: usize,
    spawn_half: f32,
    transport: gsb_server::TransportKind,
    overrides: ServerOverrides,
) -> Result<InProcessServer, gsb_server::ServerError> {
    let mut cfg = gsb_server::Config {
        bind: "127.0.0.1:0".into(),
        room_count: 1,
        visibility,
        topology,
        shard_count,
        aoi_cell_size: cell_size,
        team_vision_radius: vision_radius,
        max_snapshot_bytes,
        spawn_half_size: spawn_half,
        transport,
        ..Default::default()
    };
    apply_overrides(&mut cfg, &overrides);
    let (rep_tx, rep_rx) = mpsc::unbounded_channel::<MetricReport>();
    let handle = gsb_server::start_server_metrics(cfg, rep_tx).await?;
    Ok(InProcessServer { handle, rep_rx })
}

/// Own the report receiver in a dedicated task (its only awaited source
/// is the channel); keep every report — the peak gauges (connection
/// count) and a stable tick rate need the series, not just the last
/// (shutdown) report.
pub(crate) async fn drain_reports(mut rx: mpsc::UnboundedReceiver<MetricReport>) -> Vec<MetricReport> {
    let mut all = Vec::new();
    while let Some(r) = rx.recv().await {
        all.push(r);
    }
    all
}

pub(crate) fn init_tracing() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .init();
}
