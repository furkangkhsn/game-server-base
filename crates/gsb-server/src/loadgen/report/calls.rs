//! The call table's counters (B157), the hosted services' (B164) and the
//! event path's (round e2) on the RESULT line, right before `game=` like
//! the other segments.

use gsb_core::metrics::{CallCounts, EventCounts, MetricReport, ServiceReport};

/// ` <field>=<n>` for every `CallCounts` field (the room line's keys),
/// every key always present: zeros without a report.
pub(crate) fn calls_segment(calls: Option<&CallCounts>) -> String {
    calls
        .copied()
        .unwrap_or_default()
        .fields()
        .iter()
        .map(|(k, v)| format!(" {k}={v}"))
        .collect()
}

/// The latest report's services (cumulative, so the last report that
/// has them says everything): ` svc_<service>_<counter>=<n>` per counter,
/// then per method ` svc_<service>_<method>_calls=`, `…_latency_us_mean=`
/// and `…_latency_us_max=`. Empty for a game that hosts none.
pub(crate) fn services_segment(reports: &[MetricReport]) -> String {
    let Some(last) = reports.iter().rev().find(|r| !r.services.is_empty()) else {
        return String::new();
    };
    last.services.iter().map(service_keys).collect()
}

/// The event path (`docs/EVENTS.md` §9), every key always present
/// (zeros without a report): the room's event table (` <field>=<n>`,
/// the room line's keys; summed over shards), then the router's and the
/// connections' PUSH counters from the latest report (the `scope=events`
/// line's keys), then per publishing handle ` pub_<name>_<counter>=<n>`
/// (none for a game that minted none).
pub(crate) fn events_segment(events: Option<&EventCounts>, reports: &[MetricReport]) -> String {
    let mut seg: String = events
        .copied()
        .unwrap_or_default()
        .fields()
        .iter()
        .map(|(k, v)| format!(" {k}={v}"))
        .collect();
    let last = reports.last();
    let router = last
        .and_then(|r| r.registry)
        .map(|g| g.events)
        .unwrap_or_default();
    let pushes = last.map(|r| r.net.pushes).unwrap_or_default();
    for (k, v) in router.fields().iter().chain(pushes.fields().iter()) {
        seg.push_str(&format!(" {k}={v}"));
    }
    for p in last.map(|r| r.publishers.as_slice()).unwrap_or_default() {
        for (k, v) in p.counts.fields() {
            seg.push_str(&format!(" pub_{}_{k}={v}", p.publisher));
        }
    }
    seg
}

fn service_keys(s: &ServiceReport) -> String {
    let mut seg: String = s
        .counts
        .fields()
        .iter()
        .map(|(k, v)| format!(" svc_{}_{k}={v}", s.service))
        .collect();
    for m in &s.methods {
        seg.push_str(&format!(
            " svc_{0}_{1}_calls={2} svc_{0}_{1}_latency_us_mean={3:.1} svc_{0}_{1}_latency_us_max={4}",
            s.service,
            m.method,
            m.calls,
            m.latency_us_mean(),
            m.latency_us_max
        ));
    }
    seg
}

#[cfg(test)]
mod tests {
    use super::*;
    use gsb_core::metrics::{MethodReport, ServiceCounts};

    /// The event path's keys: the room's event table, the router's and
    /// the connections' counters always present (zeros without a
    /// report), then one `pub_<name>_<counter>=` per publisher counter.
    #[test]
    fn every_event_counter_is_a_key_and_publishers_are_named() {
        use gsb_core::metrics::{
            EVENT_COUNT, PUSH_COUNT, PublisherCounts, PublisherReport, ROUTER_COUNT,
        };
        let none = events_segment(None, &[]);
        assert_eq!(
            none.matches('=').count(),
            EVENT_COUNT + ROUTER_COUNT + PUSH_COUNT
        );
        assert!(none.starts_with(" events_admitted=0 "), "{none}");
        assert!(none.ends_with(" pushes_dropped_unread=0"), "{none}");
        let mut r = gsb_core::MetricReport::initial_stale(std::time::Duration::from_secs(1));
        r.registry = Some(gsb_core::metrics::RegistryReport {
            events: gsb_core::metrics::RouterCounts {
                events_offline: 3,
                ..Default::default()
            },
            ..Default::default()
        });
        r.net.pushes.pushes_written = 7;
        r.publishers = vec![PublisherReport {
            publisher: "bank".into(),
            id: 1,
            counts: PublisherCounts {
                events_pushed: 9,
                ..Default::default()
            },
        }];
        let ev = EventCounts {
            events_untaken: 2,
            ..Default::default()
        };
        let seg = events_segment(Some(&ev), &[r]);
        for key in [
            " events_untaken=2 ",
            " events_offline=3 ",
            " pushes_written=7 ",
            " pub_bank_events_pushed=9 ",
        ] {
            assert!(seg.contains(key), "{key} in {seg}");
        }
        assert!(
            seg.ends_with(" pub_bank_events_refused_too_large=0"),
            "{seg}"
        );
    }

    #[test]
    fn every_call_counter_is_a_key_zeros_included() {
        let none = calls_segment(None);
        assert_eq!(none.matches('=').count(), gsb_core::metrics::CALL_COUNT);
        assert!(none.starts_with(" calls_issued=0 calls_sent=0"), "{none}");
        let c = CallCounts {
            results_timed_out: 4,
            records_dropped: 1,
            ..Default::default()
        };
        let seg = calls_segment(Some(&c));
        assert!(seg.contains(" results_timed_out=4 "), "{seg}");
        assert!(seg.ends_with(" records_dropped=1"), "{seg}");
    }

    #[test]
    fn the_last_report_with_services_names_each_counter_and_method() {
        let svc = ServiceReport {
            service: "Economy".into(),
            counts: ServiceCounts {
                calls_received: 12,
                calls_shed: 2,
                ..Default::default()
            },
            methods: vec![MethodReport {
                method: "buy".into(),
                calls: 10,
                latency_us_total: 1_500,
                latency_us_max: 400,
            }],
        };
        let mut a = MetricReport::initial_stale(std::time::Duration::from_secs(1));
        let b = a.clone();
        a.services = vec![svc];
        let seg = services_segment(&[a, b]);
        assert!(
            seg.starts_with(" svc_Economy_calls_received=12 svc_Economy_calls_timed_out=0 svc_Economy_calls_shed=2"),
            "{seg}"
        );
        assert!(
            seg.ends_with(" svc_Economy_buy_calls=10 svc_Economy_buy_latency_us_mean=150.0 svc_Economy_buy_latency_us_max=400"),
            "{seg}"
        );
        assert_eq!(services_segment(&[]), "");
    }
}
