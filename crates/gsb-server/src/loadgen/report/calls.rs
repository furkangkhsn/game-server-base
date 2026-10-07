//! The call table's counters (B157) and the hosted services' (B164) on
//! the RESULT line, right before `game=` like the other segments.

use gsb_core::metrics::{CallCounts, MetricReport, ServiceReport};

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
