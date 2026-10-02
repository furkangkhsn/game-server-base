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

mod registry;
mod room;
mod server;
mod session;
mod stop;
mod transport;

pub(super) use registry::REGISTRY;
pub(super) use server::NET;
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
    room::TICK
        .iter()
        .chain(session::SESSION.iter())
        .chain(stop::STOP.iter())
}

/// The metrics channel's health (top of every exposition).
pub(super) const METRICS_DROPPED: Scalar<MetricReport> = Scalar {
    name: "gsb_metrics_dropped_total",
    kind: Kind::Counter,
    help: "Metric samples dropped on the bounded metrics channel across all producers.",
    get: |r| r.metrics_dropped,
};

/// The collector's own measure (F70): periodic reports emitted torn at
/// the cut grace (right after the channel's health).
pub(super) const REPORTS_TORN: Scalar<MetricReport> = Scalar {
    name: "gsb_metrics_reports_torn_at_cut_grace_total",
    kind: Kind::Counter,
    help: "Periodic metric reports the collector emitted torn at its cut grace (a sharded room's round still in flight when the grace ran out; the report went out, nothing was lost).",
    get: |r| r.reports_torn_at_cut_grace,
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

/// The ticket-auth families (B21), after the server closes: the
/// accepted count; ONE refusal counter with a `reason` label per
/// [`crate::auth::TicketReason`] (zeros included — the set is closed);
/// the game's own checks with a `check` label per name (present once a
/// name was counted — the set is the game's); and the names past the
/// bound (present while non-zero).
pub(super) const TICKETS_ACCEPTED: (&str, &str) = (
    "gsb_net_tickets_accepted_total",
    "Tickets a ticket-auth server accepted (AUTH succeeded), cumulative.",
);
pub(super) const TICKETS_REJECTED: (&str, &str) = (
    "gsb_net_tickets_rejected_total",
    "Tickets a ticket-auth server refused (ERROR 10, the connection stays alive), by reason, cumulative.",
);
pub(super) const TICKET_GAME_REJECTS: (&str, &str) = (
    "gsb_net_ticket_game_rejects_total",
    "Tickets the game's own check refused, by the check's name (each also in the reason game of gsb_net_tickets_rejected_total), cumulative.",
);
pub(super) const TICKET_GAME_NAMES_DROPPED: (&str, &str) = (
    "gsb_net_ticket_game_names_dropped_total",
    "Game ticket refusals whose check name did not fit the bound of 8 names (counted under the reason game, the name lost), cumulative.",
);

/// The close verdicts the server's stop kept from their connections
/// (F56): ONE counter with a `reason` label/attribute per
/// [`crate::conn::ServerClose`] — the reason the connection would have
/// booked in [`SERVER_CLOSES`], zeros included (the same closed set).
/// Present with the registry slice.
pub(super) const CLOSE_VERDICTS_LOST: (&str, &str) = (
    "gsb_registry_close_verdicts_lost_total",
    "Session-close verdicts (a room's kick or input-idle close, any server verdict) the server's stop kept from their connection, by the reason it would have booked in gsb_net_server_closes_total: still queued in the room, refused by the stopped registry, unread in its mailbox, or left in the connection's inbox behind the stop's notice (the client got ERROR 14 instead), cumulative.",
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
