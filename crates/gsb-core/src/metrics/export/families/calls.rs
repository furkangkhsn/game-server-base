//! The per-room families of the call table (`docs/SERVICES.md` §9, see
//! `crate::metrics::CallCounts`): one cumulative counter per field,
//! named after it.

use super::{RoomFamily, counter};

/// The call table's counters, in the counters' order.
pub(super) const CALLS: [RoomFamily; 24] = [
    counter(
        "gsb_room_calls_issued_total",
        "Calls written into the room's call table (a game call, a forward rule, an import), cumulative.",
        |r| r.calls.calls_issued,
    ),
    counter(
        "gsb_room_calls_sent_total",
        "Calls a service's mailbox took at the room's FLUSH, cumulative.",
        |r| r.calls.calls_sent,
    ),
    counter(
        "gsb_room_calls_refused_full_total",
        "Calls refused at FLUSH because the service's mailbox was full (they end refused at the next tick), cumulative.",
        |r| r.calls.calls_refused_full,
    ),
    counter(
        "gsb_room_calls_refused_gone_total",
        "Calls refused at FLUSH because the service was gone, cumulative.",
        |r| r.calls.calls_refused_gone,
    ),
    counter(
        "gsb_room_calls_refused_share_total",
        "Calls refused at FLUSH because the room's share of the service (its in-flight cap) was used up, cumulative.",
        |r| r.calls.calls_refused_share,
    ),
    counter(
        "gsb_room_calls_exported_total",
        "Calls carried out of the room's call table by a migration export, cumulative.",
        |r| r.calls.calls_exported,
    ),
    counter(
        "gsb_room_calls_imported_total",
        "Calls taken into the room's call table by a migration import (re-issued under their key), cumulative.",
        |r| r.calls.calls_imported,
    ),
    counter(
        "gsb_room_results_ok_total",
        "Calls that ended with the service's answer, cumulative.",
        |r| r.calls.results_ok,
    ),
    counter(
        "gsb_room_results_service_error_total",
        "Calls that ended with the service's typed error, cumulative.",
        |r| r.calls.results_service_error,
    ),
    counter(
        "gsb_room_results_refused_total",
        "Calls that ended refused (a FLUSH refusal, or the service host shed them), cumulative.",
        |r| r.calls.results_refused,
    ),
    counter(
        "gsb_room_results_timed_out_total",
        "Calls that ended timed out, cumulative.",
        |r| r.calls.results_timed_out,
    ),
    counter(
        "gsb_room_results_abandoned_total",
        "Calls the service dropped unanswered (a panic, a stop), cumulative.",
        |r| r.calls.results_abandoned,
    ),
    counter(
        "gsb_room_results_late_total",
        "Answers for a call the room's table no longer holds (exported away), applied nowhere, cumulative.",
        |r| r.calls.results_late,
    ),
    counter(
        "gsb_room_responses_delivered_total",
        "Call results handed to the game (take_resolved), cumulative.",
        |r| r.calls.responses_delivered,
    ),
    counter(
        "gsb_room_responses_unhandled_total",
        "Call results handed to the game and never taken by the end of their tick, dropped, cumulative.",
        |r| r.calls.responses_unhandled,
    ),
    counter(
        "gsb_room_responses_owner_gone_total",
        "Call results whose owner no longer existed, never handed out (an answers-bound client was still answered), cumulative.",
        |r| r.calls.responses_owner_gone,
    ),
    counter(
        "gsb_room_answers_sent_total",
        "Client answers a call's answers binding or a forward rule sent, cumulative.",
        |r| r.calls.answers_sent,
    ),
    counter(
        "gsb_room_requests_undecodable_total",
        "Requests of a registered op whose payload did not decode as its type, answered by the engine, cumulative.",
        |r| r.calls.requests_undecodable,
    ),
    counter(
        "gsb_room_requests_unbound_total",
        "Requests from a session bound to no owner, answered by the engine and never shown to the game, cumulative.",
        |r| r.calls.requests_unbound,
    ),
    counter(
        "gsb_room_requests_forwarded_total",
        "Requests a forward rule sent straight to a service, cumulative.",
        |r| r.calls.requests_forwarded,
    ),
    counter(
        "gsb_room_requests_answered_total",
        "Requests the game answered itself through the call table, cumulative.",
        |r| r.calls.requests_answered,
    ),
    counter(
        "gsb_room_requests_untaken_total",
        "Requests nobody took from the call table by the end of their tick, answered by the engine, cumulative.",
        |r| r.calls.requests_untaken,
    ),
    counter(
        "gsb_room_requests_unanswered_total",
        "Requests taken from the call table but neither answered nor attached to a call by the end of their tick, answered by the engine, cumulative.",
        |r| r.calls.requests_unanswered,
    ),
    counter(
        "gsb_room_requests_replied_late_total",
        "Replies to a request the engine had already closed, ignored, cumulative.",
        |r| r.calls.requests_replied_late,
    ),
];

#[cfg(test)]
mod tests;
