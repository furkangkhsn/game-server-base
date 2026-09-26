//! The wire tags, checked WITHOUT the `prost` derive that wrote them: a
//! tiny protobuf walker reads the encoded bytes, and every expectation
//! is a field number and wire type spelled from the upstream
//! `opentelemetry-proto` files. A wrong tag in `proto` encodes and
//! decodes fine through the same structs — only this catches it.

use super::*;
use prost::Message;
use proto::{metric::Data, number_data_point::Value};

/// One field's payload by wire type.
#[derive(Debug, PartialEq)]
enum Raw {
    Varint(u64),
    Fixed64(u64),
    Len(Vec<u8>),
}

/// The top-level fields of one message, in order.
fn fields(mut b: &[u8]) -> Vec<(u64, Raw)> {
    fn varint(b: &mut &[u8]) -> u64 {
        let (mut v, mut shift) = (0u64, 0);
        loop {
            let byte = b[0];
            *b = &b[1..];
            v |= u64::from(byte & 0x7f) << shift;
            if byte & 0x80 == 0 {
                return v;
            }
            shift += 7;
        }
    }
    let mut out = Vec::new();
    while !b.is_empty() {
        let key = varint(&mut b);
        let raw = match key & 7 {
            0 => Raw::Varint(varint(&mut b)),
            1 => {
                let (v, rest) = b.split_at(8);
                b = rest;
                Raw::Fixed64(u64::from_le_bytes(v.try_into().expect("8 bytes")))
            }
            2 => {
                let n = varint(&mut b) as usize;
                let (v, rest) = b.split_at(n);
                b = rest;
                Raw::Len(v.to_vec())
            }
            w => panic!("unexpected wire type {w}"),
        };
        out.push((key >> 3, raw));
    }
    out
}

/// The payload of the only field `n` (a LEN field).
fn sub(fs: &[(u64, Raw)], n: u64) -> Vec<(u64, Raw)> {
    let mut hits = fs.iter().filter(|(f, _)| *f == n);
    let Some((_, Raw::Len(b))) = hits.next() else {
        panic!("no LEN field {n} in {fs:?}");
    };
    assert!(hits.next().is_none(), "field {n} repeated");
    fields(b)
}

fn attr(key: &str, value: &str) -> proto::KeyValue {
    proto::KeyValue {
        key: key.into(),
        value: Some(proto::AnyValue {
            value: Some(proto::any_value::Value::StringValue(value.into())),
        }),
    }
}

fn metric(data: Data) -> Vec<(u64, Raw)> {
    let m = proto::Metric {
        name: "m".into(),
        description: String::new(),
        unit: String::new(),
        data: Some(data),
    };
    let fs = fields(&m.encode_to_vec());
    assert_eq!(fs[0], (1, Raw::Len(b"m".to_vec())), "Metric.name = 1");
    fs
}

/// Sum = Metric field 7; Sum.data_points 1, temporality 2 (varint,
/// CUMULATIVE = 2), is_monotonic 3; NumberDataPoint attributes 7,
/// start 2 / time 3 (fixed64), as_int 6 (sfixed64); KeyValue key 1 /
/// value 2; AnyValue string_value 1.
#[test]
fn a_monotonic_sum_point_carries_the_upstream_tags() {
    let point = proto::NumberDataPoint {
        attributes: vec![attr("room", "r1")],
        start_time_unix_nano: 11,
        time_unix_nano: 22,
        value: Some(Value::AsInt(33)),
        flags: 0,
    };
    let fs = metric(Data::Sum(proto::Sum {
        data_points: vec![point],
        aggregation_temporality: proto::AggregationTemporality::Cumulative as i32,
        is_monotonic: true,
    }));
    let sum = sub(&fs, 7);
    assert!(sum.contains(&(2, Raw::Varint(2))), "temporality CUMULATIVE");
    assert!(sum.contains(&(3, Raw::Varint(1))), "is_monotonic");
    let p = sub(&sum, 1);
    assert!(p.contains(&(2, Raw::Fixed64(11))));
    assert!(p.contains(&(3, Raw::Fixed64(22))));
    assert!(p.contains(&(6, Raw::Fixed64(33))), "as_int = 6, sfixed64");
    let kv = sub(&p, 7);
    assert_eq!(kv[0], (1, Raw::Len(b"room".to_vec())));
    assert_eq!(sub(&kv, 2), vec![(1, Raw::Len(b"r1".to_vec()))]);
}

/// Gauge = Metric field 5; a double point is as_double 4 (fixed64 bits).
#[test]
fn a_gauge_point_carries_the_upstream_tags() {
    let point = proto::NumberDataPoint {
        attributes: Vec::new(),
        start_time_unix_nano: 0,
        time_unix_nano: 5,
        value: Some(Value::AsDouble(1.5)),
        flags: 0,
    };
    let fs = metric(Data::Gauge(proto::Gauge {
        data_points: vec![point],
    }));
    let p = sub(&sub(&fs, 5), 1);
    assert!(p.contains(&(4, Raw::Fixed64(1.5f64.to_bits()))));
}

/// Histogram = Metric field 9; temporality 2; HistogramDataPoint
/// attributes 9, count 4 / sum 5 (fixed64), bucket_counts 6 (packed
/// fixed64), explicit_bounds 7 (packed double), min 11, max 12.
#[test]
fn a_histogram_point_carries_the_upstream_tags() {
    let point = proto::HistogramDataPoint {
        attributes: vec![attr("room", "r1")],
        start_time_unix_nano: 1,
        time_unix_nano: 2,
        count: 3,
        sum: Some(4.0),
        bucket_counts: vec![1, 2],
        explicit_bounds: vec![8.0],
        flags: 0,
        min: Some(0.5),
        max: Some(9.5),
    };
    let fs = metric(Data::Histogram(proto::Histogram {
        data_points: vec![point],
        aggregation_temporality: proto::AggregationTemporality::Cumulative as i32,
    }));
    let h = sub(&fs, 9);
    assert!(h.contains(&(2, Raw::Varint(2))));
    let p = sub(&h, 1);
    assert_eq!(sub(&p, 9)[0], (1, Raw::Len(b"room".to_vec())));
    assert!(p.contains(&(4, Raw::Fixed64(3))));
    assert!(p.contains(&(5, Raw::Fixed64(4.0f64.to_bits()))));
    let packed_counts = [1u64.to_le_bytes(), 2u64.to_le_bytes()].concat();
    assert!(p.contains(&(6, Raw::Len(packed_counts))));
    assert!(p.contains(&(7, Raw::Len(8.0f64.to_le_bytes().to_vec()))));
    assert!(p.contains(&(11, Raw::Fixed64(0.5f64.to_bits()))));
    assert!(p.contains(&(12, Raw::Fixed64(9.5f64.to_bits()))));
}

/// The envelope: request.resource_metrics 1 → resource 1 (attributes 1)
/// and scope_metrics 2 → scope 1 (name 1, version 2) and metrics 2.
#[test]
fn the_request_envelope_carries_the_upstream_tags() {
    let report = MetricReport::initial_stale(Duration::from_secs(1));
    let at = map::Stamp {
        start_unix_nano: 1,
        time_unix_nano: 2,
    };
    let req = map::request(&report, "svc", at, map::Health::default());
    let rm = sub(&fields(&req.encode_to_vec()), 1);
    let kv = sub(&sub(&rm, 1), 1);
    assert_eq!(kv[0], (1, Raw::Len(b"service.name".to_vec())));
    assert_eq!(sub(&kv, 2), vec![(1, Raw::Len(b"svc".to_vec()))]);
    let sm = sub(&rm, 2);
    let scope = sub(&sm, 1);
    assert_eq!(scope[0], (1, Raw::Len(b"gsb".to_vec())));
    let version = env!("CARGO_PKG_VERSION").as_bytes().to_vec();
    assert_eq!(scope[1], (2, Raw::Len(version)));
    let first = sm.iter().find(|(f, _)| *f == 2).expect("metrics = 2");
    let Raw::Len(b) = &first.1 else {
        panic!("metric is a message")
    };
    assert_eq!(fields(b)[0], (1, Raw::Len(b"gsb_metrics_dropped".to_vec())));
}
