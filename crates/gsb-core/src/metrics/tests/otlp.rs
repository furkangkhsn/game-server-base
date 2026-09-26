//! The OTLP exporter from the outside: the mapping of a whole report
//! (pinned, and held against the Prometheus exposition of the same
//! report), and a real push to an in-test HTTP receiver.

use super::*;
#[cfg(feature = "prometheus")]
use crate::metrics::otlp::map::{self, Health, Stamp};
use crate::metrics::otlp::proto::{self, metric::Data, number_data_point::Value};

#[cfg(feature = "prometheus")]
mod cross;
#[cfg(feature = "prometheus")]
mod golden;
mod push;

/// A fixed stamp: the mapping is a pure function of report + stamp.
#[cfg(feature = "prometheus")]
const AT: Stamp = Stamp {
    start_unix_nano: 1_000,
    time_unix_nano: 2_000,
};

/// The request's metrics (one resource, one scope).
fn metrics(req: &proto::ExportMetricsServiceRequest) -> &[proto::Metric] {
    assert_eq!(req.resource_metrics.len(), 1);
    let rm = &req.resource_metrics[0];
    assert_eq!(rm.scope_metrics.len(), 1);
    &rm.scope_metrics[0].metrics
}

/// The metric named `name`.
fn find<'a>(req: &'a proto::ExportMetricsServiceRequest, name: &str) -> &'a proto::Metric {
    metrics(req)
        .iter()
        .find(|m| m.name == name)
        .unwrap_or_else(|| panic!("no metric {name}"))
}

/// A point's attributes as `k=v,k=v`.
fn attrs(kvs: &[proto::KeyValue]) -> String {
    let one = |kv: &proto::KeyValue| {
        let v = match kv.value.as_ref().and_then(|v| v.value.as_ref()) {
            Some(proto::any_value::Value::StringValue(s)) => s.clone(),
            other => format!("{other:?}"),
        };
        format!("{}={v}", kv.key)
    };
    kvs.iter().map(one).collect::<Vec<_>>().join(",")
}

/// A number point's value as text (`int 5` / `double 1.5`).
fn number(p: &proto::NumberDataPoint) -> String {
    match p.value {
        Some(Value::AsInt(v)) => format!("int {v}"),
        Some(Value::AsDouble(v)) => format!("double {v}"),
        None => "none".to_owned(),
    }
}
