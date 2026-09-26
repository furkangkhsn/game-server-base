//! The push exporters the config asks for (`[metrics]`, docs/OPS.md
//! §6), built before anything binds: a table this build cannot serve,
//! or one the exporter refuses, stops startup with its reason.
//!
//! The pull exporter (Prometheus) needs nothing here: it is the ops
//! surface's `/metrics`, rendered from the latest report at scrape time.

use gsb_core::metrics::Exporter;

use crate::config::*;

/// The exporters to install on the collector (their push tasks are
/// spawned here; each ends when the collector drops its exporter).
pub(super) fn exporters(cfg: &Config) -> Result<Vec<Box<dyn Exporter>>, ServerError> {
    match &cfg.metrics.otlp {
        Some(section) => Ok(vec![otlp(section)?]),
        None => Ok(Vec::new()),
    }
}

#[cfg(feature = "otlp")]
fn otlp(section: &OtlpSection) -> Result<Box<dyn Exporter>, ServerError> {
    use gsb_core::metrics::otlp::{OtlpConfig, otlp};
    let config = OtlpConfig {
        endpoint: section.endpoint.clone(),
        interval: std::time::Duration::from_secs(section.interval_secs),
        service_name: section.service_name.clone(),
    };
    let (exporter, pusher) = otlp(&config).map_err(|e| ServerError::BadOtlp(e.to_string()))?;
    tokio::spawn(pusher.run());
    tracing::info!(
        endpoint = %section.endpoint,
        interval_secs = section.interval_secs,
        "otlp exporter pushing"
    );
    Ok(Box::new(exporter))
}

#[cfg(not(feature = "otlp"))]
fn otlp(_: &OtlpSection) -> Result<Box<dyn Exporter>, ServerError> {
    Err(ServerError::OtlpNotBuilt)
}
