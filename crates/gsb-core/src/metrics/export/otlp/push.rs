//! The push task: one awaited source (the hand-off), one push per
//! report, failures counted and logged once per outage.

use std::time::Duration;

use prost::Message;
use tokio::sync::mpsc;

use super::endpoint::Endpoint;
use super::map::{self, Health, Stamp};
use super::{Batch, OtlpConfig};

/// The push half of an OTLP exporter (see the parent's docs). Spawn
/// [`Self::run`]; it ends when its exporter is dropped (the collector
/// exits after its final report).
pub struct OtlpPusher {
    pub(super) rx: mpsc::Receiver<Batch>,
    endpoint: Endpoint,
    service: String,
    start_unix_nano: u64,
    timeout: Duration,
    failures: u64,
    /// Inside an outage (the last push failed): later failures log at
    /// debug, the next success logs the recovery.
    failing: bool,
}

impl OtlpPusher {
    pub(super) fn new(
        rx: mpsc::Receiver<Batch>,
        endpoint: Endpoint,
        config: &OtlpConfig,
        start_unix_nano: u64,
    ) -> Self {
        Self {
            rx,
            endpoint,
            service: config.service_name.clone(),
            start_unix_nano,
            timeout: config.interval,
            failures: 0,
            failing: false,
        }
    }

    /// Push every handed-off report until the exporter is gone.
    pub async fn run(mut self) {
        while let Some(batch) = self.rx.recv().await {
            let at = Stamp {
                start_unix_nano: self.start_unix_nano,
                time_unix_nano: batch.time_unix_nano,
            };
            let health = Health {
                reports_dropped: batch.dropped,
                push_failures: self.failures,
            };
            let body = map::request(&batch.report, &self.service, at, health).encode_to_vec();
            drop(batch);
            let outcome = tokio::time::timeout(self.timeout, self.endpoint.post(&body)).await;
            match outcome {
                Ok(Ok(())) => {
                    if self.failing {
                        self.failing = false;
                        tracing::info!(failures = self.failures, "otlp push recovered");
                    }
                }
                Ok(Err(e)) => self.failed(&e),
                Err(_) => self.failed(&"timed out"),
            }
        }
    }

    fn failed(&mut self, why: &dyn std::fmt::Display) {
        self.failures += 1;
        if self.failing {
            tracing::debug!(error = %why, failures = self.failures, "otlp push failed");
        } else {
            self.failing = true;
            tracing::warn!(error = %why, "otlp push failed; retrying with the next report");
        }
    }
}
