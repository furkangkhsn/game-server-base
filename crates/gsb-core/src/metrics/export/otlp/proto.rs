//! The OTLP metrics messages this exporter writes, as `prost` structs.
//!
//! A hand-written SUBSET of `opentelemetry-proto` (v1): the export
//! request down to the three point kinds it uses (Gauge, Sum,
//! Histogram) and the attribute values it sets. Field numbers and wire
//! types are the upstream ones (`collector/metrics/v1/metrics_service`,
//! `metrics/v1/metrics`, `common/v1/common`, `resource/v1/resource`);
//! what it never writes (exemplars, summaries, exponential histograms,
//! array, kvlist and bytes values, metric metadata) is left out, and a
//! decoder skips such fields as unknown — which is also how these
//! structs read a peer's fuller message.
//!
//! WHY by hand and not `opentelemetry-proto` + `prost-build`: the subset
//! is ~20 small messages the derive covers with the `prost` the core
//! already depends on; the upstream crate would bring the OpenTelemetry
//! SDK's dependency tree (and a codegen step) for the same bytes.

use prost::{Enumeration, Message, Oneof};

/// `opentelemetry.proto.collector.metrics.v1.ExportMetricsServiceRequest`.
#[derive(Clone, PartialEq, Message)]
pub struct ExportMetricsServiceRequest {
    #[prost(message, repeated, tag = "1")]
    pub resource_metrics: Vec<ResourceMetrics>,
}

/// `opentelemetry.proto.collector.metrics.v1.ExportMetricsServiceResponse`.
#[derive(Clone, PartialEq, Message)]
pub struct ExportMetricsServiceResponse {
    #[prost(message, optional, tag = "1")]
    pub partial_success: Option<ExportMetricsPartialSuccess>,
}

/// `opentelemetry.proto.collector.metrics.v1.ExportMetricsPartialSuccess`.
#[derive(Clone, PartialEq, Message)]
pub struct ExportMetricsPartialSuccess {
    #[prost(int64, tag = "1")]
    pub rejected_data_points: i64,
    #[prost(string, tag = "2")]
    pub error_message: String,
}

/// `opentelemetry.proto.metrics.v1.ResourceMetrics`.
#[derive(Clone, PartialEq, Message)]
pub struct ResourceMetrics {
    #[prost(message, optional, tag = "1")]
    pub resource: Option<Resource>,
    #[prost(message, repeated, tag = "2")]
    pub scope_metrics: Vec<ScopeMetrics>,
    #[prost(string, tag = "3")]
    pub schema_url: String,
}

/// `opentelemetry.proto.resource.v1.Resource`.
#[derive(Clone, PartialEq, Message)]
pub struct Resource {
    #[prost(message, repeated, tag = "1")]
    pub attributes: Vec<KeyValue>,
    #[prost(uint32, tag = "2")]
    pub dropped_attributes_count: u32,
}

/// `opentelemetry.proto.metrics.v1.ScopeMetrics`.
#[derive(Clone, PartialEq, Message)]
pub struct ScopeMetrics {
    #[prost(message, optional, tag = "1")]
    pub scope: Option<InstrumentationScope>,
    #[prost(message, repeated, tag = "2")]
    pub metrics: Vec<Metric>,
    #[prost(string, tag = "3")]
    pub schema_url: String,
}

/// `opentelemetry.proto.common.v1.InstrumentationScope`.
#[derive(Clone, PartialEq, Message)]
pub struct InstrumentationScope {
    #[prost(string, tag = "1")]
    pub name: String,
    #[prost(string, tag = "2")]
    pub version: String,
    #[prost(message, repeated, tag = "3")]
    pub attributes: Vec<KeyValue>,
    #[prost(uint32, tag = "4")]
    pub dropped_attributes_count: u32,
}

/// `opentelemetry.proto.common.v1.KeyValue`.
#[derive(Clone, PartialEq, Message)]
pub struct KeyValue {
    #[prost(string, tag = "1")]
    pub key: String,
    #[prost(message, optional, tag = "2")]
    pub value: Option<AnyValue>,
}

/// `opentelemetry.proto.common.v1.AnyValue` (the scalar arms).
#[derive(Clone, PartialEq, Message)]
pub struct AnyValue {
    #[prost(oneof = "any_value::Value", tags = "1, 2, 3, 4")]
    pub value: Option<any_value::Value>,
}

/// `AnyValue`'s `value` oneof.
pub mod any_value {
    use super::Oneof;

    #[derive(Clone, PartialEq, Oneof)]
    pub enum Value {
        #[prost(string, tag = "1")]
        StringValue(String),
        #[prost(bool, tag = "2")]
        BoolValue(bool),
        #[prost(int64, tag = "3")]
        IntValue(i64),
        #[prost(double, tag = "4")]
        DoubleValue(f64),
    }
}

/// `opentelemetry.proto.metrics.v1.Metric` (without `metadata`).
#[derive(Clone, PartialEq, Message)]
pub struct Metric {
    #[prost(string, tag = "1")]
    pub name: String,
    #[prost(string, tag = "2")]
    pub description: String,
    #[prost(string, tag = "3")]
    pub unit: String,
    #[prost(oneof = "metric::Data", tags = "5, 7, 9")]
    pub data: Option<metric::Data>,
}

/// `Metric`'s `data` oneof (the three kinds this exporter writes).
pub mod metric {
    #[derive(Clone, PartialEq, prost::Oneof)]
    pub enum Data {
        #[prost(message, tag = "5")]
        Gauge(super::Gauge),
        #[prost(message, tag = "7")]
        Sum(super::Sum),
        #[prost(message, tag = "9")]
        Histogram(super::Histogram),
    }
}

/// `opentelemetry.proto.metrics.v1.AggregationTemporality`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Enumeration)]
#[repr(i32)]
pub enum AggregationTemporality {
    Unspecified = 0,
    Delta = 1,
    Cumulative = 2,
}

/// `opentelemetry.proto.metrics.v1.Gauge`.
#[derive(Clone, PartialEq, Message)]
pub struct Gauge {
    #[prost(message, repeated, tag = "1")]
    pub data_points: Vec<NumberDataPoint>,
}

/// `opentelemetry.proto.metrics.v1.Sum`.
#[derive(Clone, PartialEq, Message)]
pub struct Sum {
    #[prost(message, repeated, tag = "1")]
    pub data_points: Vec<NumberDataPoint>,
    #[prost(enumeration = "AggregationTemporality", tag = "2")]
    pub aggregation_temporality: i32,
    #[prost(bool, tag = "3")]
    pub is_monotonic: bool,
}

/// `opentelemetry.proto.metrics.v1.Histogram`.
#[derive(Clone, PartialEq, Message)]
pub struct Histogram {
    #[prost(message, repeated, tag = "1")]
    pub data_points: Vec<HistogramDataPoint>,
    #[prost(enumeration = "AggregationTemporality", tag = "2")]
    pub aggregation_temporality: i32,
}

/// `opentelemetry.proto.metrics.v1.NumberDataPoint` (without exemplars).
#[derive(Clone, PartialEq, Message)]
pub struct NumberDataPoint {
    #[prost(message, repeated, tag = "7")]
    pub attributes: Vec<KeyValue>,
    #[prost(fixed64, tag = "2")]
    pub start_time_unix_nano: u64,
    #[prost(fixed64, tag = "3")]
    pub time_unix_nano: u64,
    #[prost(oneof = "number_data_point::Value", tags = "4, 6")]
    pub value: Option<number_data_point::Value>,
    #[prost(uint32, tag = "8")]
    pub flags: u32,
}

/// `NumberDataPoint`'s `value` oneof.
pub mod number_data_point {
    #[derive(Clone, PartialEq, prost::Oneof)]
    pub enum Value {
        #[prost(double, tag = "4")]
        AsDouble(f64),
        #[prost(sfixed64, tag = "6")]
        AsInt(i64),
    }
}

/// `opentelemetry.proto.metrics.v1.HistogramDataPoint` (without
/// exemplars).
#[derive(Clone, PartialEq, Message)]
pub struct HistogramDataPoint {
    #[prost(message, repeated, tag = "9")]
    pub attributes: Vec<KeyValue>,
    #[prost(fixed64, tag = "2")]
    pub start_time_unix_nano: u64,
    #[prost(fixed64, tag = "3")]
    pub time_unix_nano: u64,
    #[prost(fixed64, tag = "4")]
    pub count: u64,
    #[prost(double, optional, tag = "5")]
    pub sum: Option<f64>,
    #[prost(fixed64, repeated, tag = "6")]
    pub bucket_counts: Vec<u64>,
    #[prost(double, repeated, tag = "7")]
    pub explicit_bounds: Vec<f64>,
    #[prost(uint32, tag = "10")]
    pub flags: u32,
    #[prost(double, optional, tag = "11")]
    pub min: Option<f64>,
    #[prost(double, optional, tag = "12")]
    pub max: Option<f64>,
}
