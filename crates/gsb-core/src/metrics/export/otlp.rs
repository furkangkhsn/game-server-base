//! The OTLP push exporter (feature `otlp`): every `interval` the latest
//! report goes to an OpenTelemetry collector as an OTLP/HTTP protobuf
//! `POST` (`/v1/metrics`).
//!
//! Two halves, joined by a bounded hand-off:
//!
//! - [`OtlpExporter`] is the collector-side [`Exporter`]: on a report
//!   that is due it clones it into a ONE-slot channel with
//!   `try_reserve` (synchronous, never parks). A full slot means the push
//!   task is still busy with the previous report: this one is dropped
//!   and counted (`gsb_export_otlp_reports_dropped`) — harmless, as every
//!   value is cumulative or a gauge and the next due report carries it.
//! - [`OtlpPusher`] is the push task: its one awaited source is that
//!   channel; per report it maps (`map`), encodes, and posts over a plain
//!   `TcpStream` with a timeout of one interval. A failed push is counted
//!   (`gsb_export_otlp_push_failures`, carried by the next push) and
//!   logged once per outage (a warn at the first failure, an info at
//!   recovery) — never retried: the next interval's report supersedes it.
//!
//! Plain `http://` only. TLS is not spoken here (an `https://` endpoint
//! refuses to start): the intended peer is a collector or agent next to
//! the server, and TLS for the hop beyond it is that collector's job.
//! gRPC is not spoken either: OTLP/HTTP protobuf is the same message
//! over a one-request HTTP/1.1 exchange, with no HTTP/2 stack to carry.

use std::time::{Duration, Instant, SystemTime};

use tokio::sync::mpsc;

use crate::metrics::{Exporter, MetricReport};

mod endpoint;
pub mod map;
pub mod proto;
mod push;

pub use push::OtlpPusher;

use endpoint::Endpoint;

/// What an OTLP exporter pushes, where, and how often.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OtlpConfig {
    /// `http://host[:port][/path]`; an empty path is `/v1/metrics`, the
    /// port defaults to 80.
    pub endpoint: String,
    /// Push cadence (at least the collector's report period in effect —
    /// a report is pushed when one interval has passed since the last
    /// push's slot). Also each push's timeout.
    pub interval: Duration,
    /// The resource's `service.name`.
    pub service_name: String,
}

/// An OTLP configuration the exporter cannot run with.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum OtlpError {
    #[error(
        "otlp endpoint `{0}` is https: the exporter speaks plain http only — \
         point it at a local collector's OTLP/HTTP receiver (e.g. http://127.0.0.1:4318)"
    )]
    Https(String),
    #[error("otlp endpoint `{0}` is not an http://host[:port][/path] url: {1}")]
    BadEndpoint(String, &'static str),
    #[error("otlp interval must be positive")]
    ZeroInterval,
}

/// One report handed to the push task.
pub(crate) struct Batch {
    pub(crate) report: MetricReport,
    /// The report's emission, Unix nanoseconds.
    pub(crate) time_unix_nano: u64,
    /// Reports dropped on a full hand-off before this one.
    pub(crate) dropped: u64,
}

/// The hand-off's depth: one report waiting while the task pushes the
/// previous one. A deeper queue would only hold reports OLDER than the
/// one that will be due next.
const HANDOFF: usize = 1;

/// Build the exporter and its push task for `config` (spawn
/// [`OtlpPusher::run`], install the exporter on the collector).
pub fn otlp(config: &OtlpConfig) -> Result<(OtlpExporter, OtlpPusher), OtlpError> {
    if config.interval.is_zero() {
        return Err(OtlpError::ZeroInterval);
    }
    let endpoint = Endpoint::parse(&config.endpoint)?;
    let clock = WallClock::now();
    let (tx, rx) = mpsc::channel(HANDOFF);
    let exporter = OtlpExporter {
        tx,
        interval: config.interval,
        next_due: None,
        dropped: 0,
        clock,
    };
    let pusher = OtlpPusher::new(rx, endpoint, config, clock.start_unix_nano);
    Ok((exporter, pusher))
}

/// The collector-side half (see the module docs).
pub struct OtlpExporter {
    tx: mpsc::Sender<Batch>,
    interval: Duration,
    /// The next push slot: on a fixed grid from the first report, so a
    /// report a hair early (the collector's own jitter) waits for the
    /// next one instead of stretching the cadence.
    next_due: Option<Instant>,
    dropped: u64,
    clock: WallClock,
}

impl OtlpExporter {
    /// Reports dropped on a full (or closed) hand-off so far.
    pub fn dropped(&self) -> u64 {
        self.dropped
    }
}

impl Exporter for OtlpExporter {
    fn export(&mut self, report: &MetricReport) {
        let at = report.emitted_at;
        let mut due = self.next_due.unwrap_or(at);
        if at < due {
            return;
        }
        while due <= at {
            due += self.interval;
        }
        self.next_due = Some(due);
        match self.tx.try_reserve() {
            Ok(slot) => slot.send(Batch {
                report: report.clone(),
                time_unix_nano: self.clock.unix_nano_at(at),
                dropped: self.dropped,
            }),
            Err(_) => self.dropped += 1,
        }
    }
}

/// The wall clock at the exporter's birth, to stamp the monotonic
/// report instants in Unix time (and to start the cumulative window).
#[derive(Debug, Clone, Copy)]
struct WallClock {
    at: Instant,
    start_unix_nano: u64,
}

impl WallClock {
    fn now() -> Self {
        let since_epoch = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default();
        Self {
            at: Instant::now(),
            start_unix_nano: nanos(since_epoch),
        }
    }

    fn unix_nano_at(&self, t: Instant) -> u64 {
        let since = nanos(t.saturating_duration_since(self.at));
        self.start_unix_nano.saturating_add(since)
    }
}

fn nanos(d: Duration) -> u64 {
    u64::try_from(d.as_nanos()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests;
