//! The public ways to start a server. The `start_server*` family picks
//! the game by the config's `game` key among the compiled-in games; the
//! `start_game_server*` pair hosts a module the caller built (a
//! third-party game, GAME-MODULE §4.2). All of them funnel into
//! `start_inner`.

use tokio::sync::mpsc;

use super::start_inner;
use crate::*;
use gsb_core::metrics::{MetricReport, MetricSink};

/// Start the server (local auth; no ticket hook). Must be called from
/// inside a tokio runtime. Metric reports go to the tracing logger (one
/// `gsb-metric` line per scope per second; visible under `RUST_LOG=info`,
/// silent without a subscriber). The hosted game is the one the config's
/// `game` key names (default: the 2D demo); an unknown name refuses
/// startup with [`GameError::Unknown`].
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
    let module = games::by_name(&cfg.game)?;
    start_inner(module, cfg, MetricSink::Log, hooks).await
}

/// Start the server with the platform's hooks and a programmatic metrics
/// consumer (the [`start_server_with`] + [`start_server_metrics`]
/// composition; see both).
pub async fn start_server_metrics_with(
    cfg: Config,
    hooks: ServerHooks,
    report_tx: mpsc::UnboundedSender<MetricReport>,
) -> Result<ServerHandle, ServerError> {
    let module = games::by_name(&cfg.game)?;
    start_inner(module, cfg, MetricSink::Channel(report_tx), hooks).await
}

/// Host `module` (unconfigured; the server configures it) under `cfg`,
/// with local auth and the log metrics sink. The explicit module wins over
/// the `game` key's default; a config FILE that writes a different
/// `game` refuses startup ([`GameError::NameMismatch`]) rather than
/// silently hosting something else.
pub async fn start_game_server(
    module: Box<dyn GameModule>,
    cfg: Config,
) -> Result<ServerHandle, ServerError> {
    start_game_server_with(module, cfg, ServerHooks::default(), None).await
}

/// [`start_game_server`] with the platform's hooks and, when `report_tx`
/// is set, a programmatic metrics consumer (as [`start_server_metrics`]).
pub async fn start_game_server_with(
    module: Box<dyn GameModule>,
    cfg: Config,
    hooks: ServerHooks,
    report_tx: Option<mpsc::UnboundedSender<MetricReport>>,
) -> Result<ServerHandle, ServerError> {
    if let Some(configured) = cfg.raw.get("game").and_then(|v| v.as_str())
        && configured != module.name()
    {
        return Err(GameError::NameMismatch {
            configured: configured.to_string(),
            module: module.name(),
        }
        .into());
    }
    let sink = match report_tx {
        Some(tx) => MetricSink::Channel(tx),
        None => MetricSink::Log,
    };
    start_inner(module, cfg, sink, hooks).await
}
