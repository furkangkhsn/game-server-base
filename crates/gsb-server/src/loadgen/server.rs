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
    /// The writer's stall window (`None` = config default; `0` = off).
    pub(crate) write_stall_secs: Option<f64>,
    /// The demo rooms' disconnect-park grace (`None` = config default).
    pub(crate) disconnect_grace_secs: Option<f64>,
    /// The MMO's `[mmo] crystallize` (`None` = the MMO's default, on).
    pub(crate) mmo_crystallize: Option<bool>,
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
    if let Some(s) = o.write_stall_secs {
        cfg.write_stall_secs = s;
    }
    if let Some(s) = o.disconnect_grace_secs {
        cfg.disconnect_grace_secs = s.max(0.0);
    }
    if let Some(on) = o.mmo_crystallize {
        // A game setting lives in the raw table the module reads (a
        // code-built config has no file behind it).
        let mmo = cfg
            .raw
            .entry("mmo")
            .or_insert_with(|| toml::Value::Table(toml::Table::new()));
        if let Some(t) = mmo.as_table_mut() {
            t.insert("crystallize".into(), toml::Value::Boolean(on));
        }
    }
}

/// Start the server in-process with a channel metrics sink. The receiver
/// moves into the report-drain task; nothing is shared across tasks
/// beyond that mailbox. `game` is the hosted game (the `game` key; the
/// flat keys below are the demo's, and a code-built config writes none
/// of them explicitly, so the other games keep their own); for the demo
/// `visibility` selects the room strategy (the five: `()` / `Cell` /
/// `Team` / `Sector` / sharded-grid).
#[allow(clippy::too_many_arguments)] // loadgen helper; params are natural
pub(crate) async fn start_inprocess(
    game: &'static str,
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
        game: game.into(),
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
pub(crate) async fn drain_reports(
    mut rx: mpsc::UnboundedReceiver<MetricReport>,
) -> Vec<MetricReport> {
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

#[cfg(test)]
mod tests {
    use super::*;

    /// `--mmo-crystallize` lands in the raw `[mmo]` table the MMO module
    /// reads; unset, the table is not touched.
    #[test]
    fn the_mmo_crystallize_override_writes_the_mmo_table() {
        let none = ServerOverrides {
            max_players: None,
            max_connections: None,
            idle_timeout_secs: None,
            write_stall_secs: None,
            disconnect_grace_secs: None,
            mmo_crystallize: None,
        };
        let mut cfg = gsb_server::Config::default();
        apply_overrides(&mut cfg, &none);
        assert!(cfg.raw.is_empty());
        for on in [false, true] {
            let o = ServerOverrides {
                mmo_crystallize: Some(on),
                ..none
            };
            apply_overrides(&mut cfg, &o);
            let got = cfg.raw["mmo"]["crystallize"].as_bool();
            assert_eq!(got, Some(on));
        }
    }
}
