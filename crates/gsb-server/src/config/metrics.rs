//! `[metrics]`: how the reports leave the server beyond the log lines
//! and the ops surface's `/metrics` (docs/OPS.md §6, the export layer).
//!
//! Today one table: `[metrics.otlp]`, the OTLP push exporter. The table
//! PARSES in every build, so a build without the `otlp` cargo feature
//! can refuse it at startup by name instead of silently ignoring a key
//! the operator wrote (see `ServerError::OtlpNotBuilt`).

/// The `[metrics]` table (empty by default: no push exporter).
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct MetricsConfig {
    /// `[metrics.otlp]`: push every report interval to an OpenTelemetry
    /// collector as OTLP/HTTP protobuf. Absent (the default) = no push.
    pub otlp: Option<OtlpSection>,
}

/// `[metrics.otlp]`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OtlpSection {
    /// `http://host[:port][/path]` of the collector's OTLP/HTTP receiver
    /// (`/v1/metrics` when the path is empty; port 80 when omitted).
    /// Plain http only: `https://` refuses startup — run a collector or
    /// agent next to the server and let it carry TLS onward.
    pub endpoint: String,
    /// Seconds between pushes (default 10; `0` refuses startup). Each
    /// push also times out after one interval. The collector reports
    /// once a second, so an interval below that pushes every report.
    #[serde(default = "default_interval_secs")]
    pub interval_secs: u64,
    /// The resource's `service.name` (default `"gsb"`).
    #[serde(default = "default_service_name")]
    pub service_name: String,
}

fn default_interval_secs() -> u64 {
    10
}

fn default_service_name() -> String {
    "gsb".to_owned()
}

#[cfg(test)]
mod tests;
