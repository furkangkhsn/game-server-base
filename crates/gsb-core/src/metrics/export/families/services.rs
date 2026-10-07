//! The service-scoped families (BACKLOG B164, see
//! `crate::metrics::ServiceCounts`): one cumulative counter per
//! `ServiceStats` field, the service as its label (`service="<name>"`),
//! then each method's finished calls and latency (`service`, `method`).
//! Each loss has a counter of its own (B32).

use super::{MethodFamily, MethodValue, ServiceFamily};
use crate::metrics::SERVICE_COUNT;

/// The service counters, in the counters' order.
pub(in crate::metrics::export) const SERVICE: [ServiceFamily; SERVICE_COUNT] = [
    ServiceFamily {
        name: "gsb_service_calls_received_total",
        help: "Calls the service took out of its mailbox, cumulative.",
        get: |c| c.calls_received,
    },
    ServiceFamily {
        name: "gsb_service_calls_timed_out_total",
        help: "Calls the service host's timeout ended (the caller got TimedOut), cumulative.",
        get: |c| c.calls_timed_out,
    },
    ServiceFamily {
        name: "gsb_service_calls_shed_total",
        help: "Calls the service host's load-shedding layer refused (the caller got Refused(Shed)), cumulative.",
        get: |c| c.calls_shed,
    },
    ServiceFamily {
        name: "gsb_service_calls_failed_total",
        help: "Calls another layer of the service host's stack failed (the caller got Abandoned), cumulative.",
        get: |c| c.calls_failed,
    },
    ServiceFamily {
        name: "gsb_service_calls_abandoned_total",
        help: "Calls whose service task ended without answering (a panic in the service; the caller got Abandoned), cumulative.",
        get: |c| c.calls_abandoned,
    },
    ServiceFamily {
        name: "gsb_service_replies_undeliverable_full_total",
        help: "Service answers the caller's full completion channel refused (lost), cumulative.",
        get: |c| c.replies_undeliverable_full,
    },
    ServiceFamily {
        name: "gsb_service_replies_undeliverable_gone_total",
        help: "Service answers whose caller was gone (its call table dropped, or an awaiting handle stopped waiting; lost), cumulative.",
        get: |c| c.replies_undeliverable_gone,
    },
];

/// The per-method families.
pub(in crate::metrics::export) const METHOD: [MethodFamily; 3] = [
    MethodFamily {
        name: "gsb_service_method_calls_total",
        help: "Calls of the service method that finished (every ending), cumulative.",
        value: MethodValue::Counter(|m| m.calls),
    },
    MethodFamily {
        name: "gsb_service_method_latency_us_total",
        help: "Summed service-side latency of the method's finished calls (mailbox take to answer), microseconds, cumulative (divide by gsb_service_method_calls_total for the mean).",
        value: MethodValue::Counter(|m| m.latency_us_total),
    },
    MethodFamily {
        name: "gsb_service_method_latency_us_max",
        help: "The largest service-side latency of one of the method's calls, microseconds, since startup.",
        value: MethodValue::Gauge(|m| m.latency_us_max),
    },
];
