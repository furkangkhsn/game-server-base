//! The metric vocabulary every exporter shares: each family's name,
//! type, help and how it reads its value from a [`MetricReport`].
//!
//! One table, several formats: the Prometheus exposition and the OTLP
//! push walk the SAME families in the same order, so a counter added
//! here is exported by every compiled-in exporter at once, and the two
//! surfaces cannot drift apart in naming or type. Names are the
//! Prometheus spelling (`gsb_<scope>_<counter>`, `_total` on a counter);
//! an exporter whose format spells them differently derives its name
//! from this one (the OTLP name drops `_total`, see `otlp`).
//!
//! What stays with a format: how a distribution is shaped (the log2
//! histogram's per-room `le` edges, the fine histogram as a summary or a
//! histogram), and the families whose SET of names is only known at
//! report time (the logic's own counters — their rule is still here).

use crate::metrics::{MetricReport, RoomReport};

mod room;
mod server;
mod session;
mod transport;

pub(super) use server::{NET, REGISTRY};
pub(super) use transport::TRANSPORT;

/// A family's type: a cumulative counter or a gauge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Kind {
    /// Cumulative since startup, never decreases.
    Counter,
    /// A value that moves both ways (a current count, a high-water mark,
    /// a rate over the last interval).
    Gauge,
}

#[cfg(feature = "prometheus")]
impl Kind {
    /// The Prometheus `# TYPE` word.
    pub(super) fn word(self) -> &'static str {
        match self {
            Kind::Counter => "counter",
            Kind::Gauge => "gauge",
        }
    }
}

/// One server-wide family: a single unlabeled value read from `T`.
pub(super) struct Scalar<T> {
    pub(super) name: &'static str,
    pub(super) kind: Kind,
    pub(super) help: &'static str,
    pub(super) get: fn(&T) -> u64,
}

/// How a per-room family reads its value (one sample per room, the
/// room as its label/attribute).
#[derive(Clone, Copy)]
pub(super) enum RoomValue {
    /// A cumulative integer counter.
    Counter(fn(&RoomReport) -> u64),
    /// A gauge (current values, extremes, rates, means).
    Gauge(fn(&RoomReport) -> f64),
    /// The budget-relative log2 step histogram (`RoomReport::step_hist`).
    StepHist,
    /// The fine fixed-bin step histogram (`RoomReport::step_fine_hist`).
    StepFine,
}

/// One per-room family.
pub(super) struct RoomFamily {
    pub(super) name: &'static str,
    pub(super) help: &'static str,
    pub(super) value: RoomValue,
}

/// A per-room counter (constructor for the tables).
const fn counter(
    name: &'static str,
    help: &'static str,
    get: fn(&RoomReport) -> u64,
) -> RoomFamily {
    RoomFamily {
        name,
        help,
        value: RoomValue::Counter(get),
    }
}

/// A per-room gauge (constructor for the tables).
const fn gauge(name: &'static str, help: &'static str, get: fn(&RoomReport) -> f64) -> RoomFamily {
    RoomFamily {
        name,
        help,
        value: RoomValue::Gauge(get),
    }
}

/// Every per-room family, in exposition order.
pub(super) fn room() -> impl Iterator<Item = &'static RoomFamily> {
    room::TICK.iter().chain(session::SESSION.iter())
}

/// The metrics channel's health (top of every exposition).
pub(super) const METRICS_DROPPED: Scalar<MetricReport> = Scalar {
    name: "gsb_metrics_dropped_total",
    kind: Kind::Counter,
    help: "Metric samples dropped on the bounded metrics channel across all producers.",
    get: |r| r.metrics_dropped,
};

/// The server-close family: ONE counter with a `reason` label/attribute
/// per [`crate::conn::ServerClose`] — zeros included (the set is closed
/// and known; a series that appears only once non-zero cannot be rated
/// from its first increment).
pub(super) const SERVER_CLOSES: (&str, &str) = (
    "gsb_net_server_closes_total",
    "Sessions the server ended on its own initiative, by reason \
     (client-initiated closes are not counted), cumulative.",
);

/// The logic's own counters (F9): the default help of a SUM (a
/// counter) and of a MAX (a gauge: a high-water mark), used when the
/// declaration brings none.
pub(super) const LOGIC_SUM_HELP: &str = "The logic's own counter, cumulative.";
pub(super) const LOGIC_MAX_HELP: &str = "The logic's own high-water mark.";

/// The bound's overflow (F17): a gauge per room whose latest sample
/// dropped names — present only while non-zero.
pub(super) const LOGIC_DROPPED: (&str, &str) = (
    "gsb_room_logic_counters_dropped",
    "Logic counter values the room's latest sample dropped: names beyond the per-room bound of 16.",
);
