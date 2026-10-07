//! The services' own counters (BACKLOG B164, `docs/SERVICES.md` §9):
//! what each hosted service's [`ServiceStats`] says — the calls it took,
//! and every way a call or its answer was lost on the service's side,
//! each in a counter of its own ("count every loss", B32) — plus each
//! method's finished calls and latency. Service-scoped, not per room: a
//! service serves every room.
//!
//! The collector reads the stats it was handed
//! ([`MetricsCollector::with_services`](crate::metrics::MetricsCollector::with_services))
//! at each report — atomic loads, no message, no await.

use std::sync::atomic::Ordering;

use crate::calls::ServiceStats;

/// Declares [`ServiceCounts`] and its whole-set operations (the
/// `CallCounts` pattern: a field added to the list cannot be left out of
/// one of them).
macro_rules! service_counts {
    ($( $(#[$doc:meta])* $field:ident, )*) => {
        /// One service's counters (see the module docs).
        #[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
        pub struct ServiceCounts {
            $( $(#[$doc])* pub $field: u64, )*
        }

        impl ServiceCounts {
            /// Every field as `(name, value)`, in declaration order (the
            /// log line's keys, the RESULT keys, the loadgen wire's
            /// order).
            pub fn fields(&self) -> [(&'static str, u64); SERVICE_COUNT] {
                [ $( (stringify!($field), self.$field), )* ]
            }

            /// The inverse of [`Self::fields`]' values (the loadgen wire).
            pub fn from_values(v: [u64; SERVICE_COUNT]) -> Self {
                let mut i = 0;
                $( let $field = v[i]; i += 1; )*
                let _ = i;
                Self { $( $field, )* }
            }

            /// A service's stats, read now.
            pub fn read(s: &ServiceStats) -> Self {
                Self { $( $field: s.$field.load(Ordering::Relaxed), )* }
            }
        }

        /// How many counters [`ServiceCounts`] has.
        pub const SERVICE_COUNT: usize = [$( stringify!($field), )*].len();
    };
}

service_counts! {
    /// Calls the service took out of its mailbox.
    calls_received,
    /// Calls the host's timeout ended (`TimedOut`).
    calls_timed_out,
    /// Calls the host's load-shedding layer refused (`Refused(Shed)`).
    calls_shed,
    /// Calls another layer of the host's stack failed (`Abandoned`).
    calls_failed,
    /// Calls whose service task ended without answering (a panic:
    /// `Abandoned`).
    calls_abandoned,
    /// Answers the caller's full completion channel refused: lost.
    replies_undeliverable_full,
    /// Answers whose caller was gone (its table dropped, or an awaiting
    /// handle stopped waiting): lost.
    replies_undeliverable_gone,
}

/// One method's finished calls and their service-side latency (from the
/// host taking the call out of its mailbox to its answer).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MethodReport {
    /// The method's name.
    pub method: String,
    /// Calls finished (every ending).
    pub calls: u64,
    /// Their summed latency, microseconds (cumulative).
    pub latency_us_total: u64,
    /// The largest latency seen, microseconds.
    pub latency_us_max: u64,
}

impl MethodReport {
    /// The mean latency, microseconds (0 before the first call).
    pub fn latency_us_mean(&self) -> f64 {
        if self.calls == 0 {
            0.0
        } else {
            self.latency_us_total as f64 / self.calls as f64
        }
    }
}

/// One service's slice of a report.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ServiceReport {
    /// The service's name (the trait's, `ServiceKind::NAME`).
    pub service: String,
    /// Its counters (cumulative).
    pub counts: ServiceCounts,
    /// Its methods, in declaration order (none for a hand-written
    /// service).
    pub methods: Vec<MethodReport>,
}

impl ServiceReport {
    /// `service`'s stats, read now.
    pub fn read(service: &str, s: &ServiceStats) -> Self {
        Self {
            service: service.to_owned(),
            counts: ServiceCounts::read(s),
            methods: s
                .methods
                .iter()
                .map(|m| MethodReport {
                    method: m.name.to_owned(),
                    calls: m.calls.load(Ordering::Relaxed),
                    latency_us_total: m.latency_us_total.load(Ordering::Relaxed),
                    latency_us_max: m.latency_us_max.load(Ordering::Relaxed),
                })
                .collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::Ordering::Relaxed;
    use std::time::Instant;

    use crate::calls::{MethodStats, ServiceStats};
    use crate::metrics::MetricAccumulator;

    /// Each report reads the stats as they are then: every counter in its
    /// own field, the methods in order, cumulative.
    #[test]
    fn every_report_reads_the_services_stats_as_they_are() {
        let stats = Arc::new(ServiceStats {
            methods: vec![MethodStats {
                name: "buy",
                ..Default::default()
            }]
            .into(),
            ..Default::default()
        });
        let mut acc =
            MetricAccumulator::default().with_services(vec![("Economy", Arc::clone(&stats))]);
        let r = acc.report(Instant::now());
        assert_eq!(r.services.len(), 1);
        assert_eq!(r.services[0].counts, Default::default());
        stats.calls_received.store(5, Relaxed);
        stats.calls_shed.store(2, Relaxed);
        stats.replies_undeliverable_gone.store(1, Relaxed);
        stats.methods[0].record(300);
        stats.methods[0].record(100);
        let r = acc.report(Instant::now());
        let s = &r.services[0];
        assert_eq!(s.service, "Economy");
        assert_eq!(
            (
                s.counts.calls_received,
                s.counts.calls_shed,
                s.counts.replies_undeliverable_gone
            ),
            (5, 2, 1)
        );
        assert_eq!(s.counts.calls_timed_out + s.counts.calls_failed, 0);
        let m = &s.methods[0];
        assert_eq!(
            (
                m.method.as_str(),
                m.calls,
                m.latency_us_total,
                m.latency_us_max
            ),
            ("buy", 2, 400, 300)
        );
        assert_eq!(m.latency_us_mean(), 200.0);
    }
}
