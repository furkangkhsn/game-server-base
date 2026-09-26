//! The constructors of the request's pieces: a metric of each kind,
//! its points, and the attributes they carry.

use super::Stamp;
use super::proto::{self, metric::Data, number_data_point::Value};
use crate::metrics::RoomReport;
use crate::metrics::export::families::Kind;

/// An unlabeled server-wide value by its kind.
pub(super) fn scalar(name: &str, kind: Kind, help: &str, v: u64, at: Stamp) -> proto::Metric {
    match kind {
        Kind::Counter => sum(name, help, vec![int_point(Vec::new(), v, at, true)]),
        Kind::Gauge => gauge(name, help, vec![int_point(Vec::new(), v, at, false)]),
    }
}

pub(super) fn sum(
    name: &str,
    help: &str,
    data_points: Vec<proto::NumberDataPoint>,
) -> proto::Metric {
    let data = Data::Sum(proto::Sum {
        data_points,
        aggregation_temporality: proto::AggregationTemporality::Cumulative as i32,
        is_monotonic: true,
    });
    metric(name, help, data)
}

pub(super) fn gauge(
    name: &str,
    help: &str,
    data_points: Vec<proto::NumberDataPoint>,
) -> proto::Metric {
    metric(name, help, Data::Gauge(proto::Gauge { data_points }))
}

pub(super) fn metric(name: &str, help: &str, data: Data) -> proto::Metric {
    proto::Metric {
        name: name.strip_suffix("_total").unwrap_or(name).to_owned(),
        description: help.to_owned(),
        unit: String::new(),
        data: Some(data),
    }
}

/// An integer point; a cumulative one (a sum's) carries the window's
/// start, a gauge's does not.
pub(super) fn int_point(
    attributes: Vec<proto::KeyValue>,
    v: u64,
    at: Stamp,
    cumulative: bool,
) -> proto::NumberDataPoint {
    proto::NumberDataPoint {
        attributes,
        start_time_unix_nano: if cumulative { at.start_unix_nano } else { 0 },
        time_unix_nano: at.time_unix_nano,
        // OTLP integers are signed: a count past i64::MAX saturates.
        value: Some(Value::AsInt(i64::try_from(v).unwrap_or(i64::MAX))),
        flags: 0,
    }
}

pub(super) fn double_point(
    attribute: proto::KeyValue,
    v: f64,
    at: Stamp,
) -> proto::NumberDataPoint {
    proto::NumberDataPoint {
        attributes: vec![attribute],
        start_time_unix_nano: 0,
        time_unix_nano: at.time_unix_nano,
        value: Some(Value::AsDouble(v)),
        flags: 0,
    }
}

/// The room attribute: the exposition's label value, `r<id>`.
pub(super) fn room(r: &RoomReport) -> proto::KeyValue {
    attr("room", &format!("r{}", r.room.0))
}

pub(super) fn attr(key: &str, value: &str) -> proto::KeyValue {
    proto::KeyValue {
        key: key.to_owned(),
        value: Some(proto::AnyValue {
            value: Some(proto::any_value::Value::StringValue(value.to_owned())),
        }),
    }
}
