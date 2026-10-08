//! The event path's families (`docs/EVENTS.md` §9, round e2): one
//! cumulative counter per field of each counter set, named after it —
//!
//! - per room: the event table and its EVENTS phase
//!   (`gsb_room_events_*_total`, [`EventCounts`](crate::metrics::EventCounts));
//! - the registry's router (`gsb_events_*_total`,
//!   [`RouterCounts`](crate::metrics::RouterCounts)), with the registry
//!   slice;
//! - the connections' PUSH frames (`gsb_net_pushes_*_total`,
//!   [`PushCounts`](crate::metrics::PushCounts));
//! - per publishing handle (`gsb_publisher_events_*_total`, the
//!   publisher's name as its label, [`PublisherCounts`](crate::metrics::PublisherCounts)).

use super::{Kind, RoomFamily, Scalar, counter};
use crate::metrics::{
    EVENT_COUNT, NetReport, PUBLISHER_COUNT, PUSH_COUNT, PublisherCounts, ROUTER_COUNT,
    RegistryReport,
};

/// One publisher-scoped counter: a sample per publishing handle, its name
/// as the label/attribute (`publisher="<name>"`).
pub(in crate::metrics::export) struct PublisherFamily {
    pub(in crate::metrics::export) name: &'static str,
    pub(in crate::metrics::export) help: &'static str,
    pub(in crate::metrics::export) get: fn(&PublisherCounts) -> u64,
}

/// The room's event table, in the counters' order.
pub(super) const EVENTS: [RoomFamily; EVENT_COUNT] = [
    counter(
        "gsb_room_events_admitted_total",
        "Event WORLD legs that entered the room's event table (decoded, owner resolved), cumulative.",
        |r| r.events.events_admitted,
    ),
    counter(
        "gsb_room_events_no_handler_total",
        "Event WORLD legs of an op the room never accepted, dropped, cumulative.",
        |r| r.events.events_no_handler,
    ),
    counter(
        "gsb_room_events_undecodable_total",
        "Event WORLD legs whose body did not decode as the accepted type, dropped, cumulative.",
        |r| r.events.events_undecodable,
    ),
    counter(
        "gsb_room_events_unbound_total",
        "Event WORLD legs for a member bound to no owner, dropped, cumulative.",
        |r| r.events.events_unbound,
    ),
    counter(
        "gsb_room_events_taken_total",
        "Events the room's game took from its event table, cumulative.",
        |r| r.events.events_taken,
    ),
    counter(
        "gsb_room_events_untaken_total",
        "Events nobody took by the end of their tick, dropped, cumulative.",
        |r| r.events.events_untaken,
    ),
    counter(
        "gsb_room_events_world_not_here_total",
        "Event WORLD legs for an identity that was no member of the room (or shard) by its EVENTS phase, dropped, cumulative.",
        |r| r.events.events_world_not_here,
    ),
    counter(
        "gsb_room_events_forwarded_total",
        "Event WORLD legs a shard handed on to the shard its member migrated to (a transfer, not an ending), cumulative.",
        |r| r.events.events_forwarded,
    ),
    counter(
        "gsb_room_events_forward_refused_total",
        "Event WORLD legs a migration's tombstone could not hand on (the target shard's link full or closed), dropped, cumulative.",
        |r| r.events.events_forward_refused,
    ),
    counter(
        "gsb_room_events_fence_held_total",
        "Event WORLD legs held behind a migration's fence, cumulative.",
        |r| r.events.events_fence_held,
    ),
    counter(
        "gsb_room_events_fence_released_total",
        "Held event WORLD legs a migration's fence (or its timeout) released, cumulative.",
        |r| r.events.events_fence_released,
    ),
    counter(
        "gsb_room_events_fence_overflow_total",
        "Event WORLD legs dropped because a migration fence's hold was full, cumulative.",
        |r| r.events.events_fence_overflow,
    ),
    counter(
        "gsb_room_events_fence_timeouts_total",
        "Migration fences that never came in time (held legs released without the order guarantee), cumulative.",
        |r| r.events.events_fence_timeouts,
    ),
    counter(
        "gsb_room_events_fence_lost_total",
        "Migration seals and fences nobody waited for, cumulative.",
        |r| r.events.events_fence_lost,
    ),
    counter(
        "gsb_room_events_relocations_refused_total",
        "Shard migration reports (Relocated) the registry's mailbox refused, cumulative.",
        |r| r.events.events_relocations_refused,
    ),
    counter(
        "gsb_room_events_tombstones_expired_total",
        "Migration tombstones that expired, cumulative.",
        |r| r.events.events_tombstones_expired,
    ),
];

const fn router(
    name: &'static str,
    help: &'static str,
    get: fn(&RegistryReport) -> u64,
) -> Scalar<RegistryReport> {
    Scalar {
        name,
        kind: Kind::Counter,
        help,
        get,
    }
}

/// The router's counters, in their order (with the registry slice).
pub(in crate::metrics::export) const ROUTER: [Scalar<RegistryReport>; ROUTER_COUNT] = [
    router(
        "gsb_events_routed_total",
        "Pushes the event router took from its mailbox, cumulative.",
        |r| r.events.events_routed,
    ),
    router(
        "gsb_events_unread_total",
        "Pushes still in the event router's mailbox when it stopped, never routed, cumulative.",
        |r| r.events.events_unread,
    ),
    router(
        "gsb_events_offline_total",
        "Identity-addressed push recipients with no authenticated session and nothing parked, cumulative.",
        |r| r.events.events_offline,
    ),
    router(
        "gsb_events_client_sent_total",
        "Event CLIENT legs handed to a connection actor, cumulative.",
        |r| r.events.events_client_sent,
    ),
    router(
        "gsb_events_client_parked_total",
        "Event CLIENT legs for a parked identity (no live session), lost, cumulative.",
        |r| r.events.events_client_parked,
    ),
    router(
        "gsb_events_client_unsupported_total",
        "Event CLIENT legs for a session whose client did not announce PUSH, lost, cumulative.",
        |r| r.events.events_client_unsupported,
    ),
    router(
        "gsb_events_client_refused_conn_full_total",
        "Event CLIENT legs a connection actor's full inbox refused, lost, cumulative.",
        |r| r.events.events_client_refused_conn_full,
    ),
    router(
        "gsb_events_client_refused_conn_gone_total",
        "Event CLIENT legs for a connection actor that was gone, lost, cumulative.",
        |r| r.events.events_client_refused_conn_gone,
    ),
    router(
        "gsb_events_world_sent_total",
        "Event WORLD legs handed to a room's or shard's event mailbox, cumulative.",
        |r| r.events.events_world_sent,
    ),
    router(
        "gsb_events_world_no_room_total",
        "Event WORLD legs for an identity in no room (or a room-wide leg for an unknown room), lost, cumulative.",
        |r| r.events.events_world_no_room,
    ),
    router(
        "gsb_events_world_refused_room_full_total",
        "Event WORLD legs a room's full event mailbox refused, lost, cumulative.",
        |r| r.events.events_world_refused_room_full,
    ),
    router(
        "gsb_events_world_refused_room_gone_total",
        "Event WORLD legs whose room had ended, lost, cumulative.",
        |r| r.events.events_world_refused_room_gone,
    ),
    router(
        "gsb_events_relocations_total",
        "Shard migrations that moved an identity's event WORLD legs to its new shard, cumulative.",
        |r| r.events.events_relocations,
    ),
    router(
        "gsb_events_relocations_stale_total",
        "Shard migration reports the event router did not apply (stale count, ended room, no membership), cumulative.",
        |r| r.events.events_relocations_stale,
    ),
    router(
        "gsb_events_seals_sent_total",
        "Migration seals sent to a migration's source shard, cumulative.",
        |r| r.events.events_seals_sent,
    ),
    router(
        "gsb_events_seals_refused_total",
        "Migration seals the source shard's event mailbox refused, cumulative.",
        |r| r.events.events_seals_refused,
    ),
];

const fn pushes(
    name: &'static str,
    help: &'static str,
    get: fn(&NetReport) -> u64,
) -> Scalar<NetReport> {
    Scalar {
        name,
        kind: Kind::Counter,
        help,
        get,
    }
}

/// The connections' PUSH frames, in the counters' order.
pub(in crate::metrics::export) const PUSHES: [Scalar<NetReport>; PUSH_COUNT] = [
    pushes(
        "gsb_net_pushes_written_total",
        "PUSH frames (event CLIENT legs) queued on a connection's outbound channel, cumulative.",
        |n| n.pushes.pushes_written,
    ),
    pushes(
        "gsb_net_pushes_dropped_outbound_full_total",
        "PUSH frames a connection's full outbound channel refused (a slow client), lost, cumulative.",
        |n| n.pushes.pushes_dropped_outbound_full,
    ),
    pushes(
        "gsb_net_pushes_dropped_outbound_gone_total",
        "PUSH frames for a connection whose writer was gone, lost, cumulative.",
        |n| n.pushes.pushes_dropped_outbound_gone,
    ),
    pushes(
        "gsb_net_pushes_dropped_unread_total",
        "Event CLIENT legs still in a connection actor's inbox when its session ended, lost, cumulative.",
        |n| n.pushes.pushes_dropped_unread,
    ),
];

/// The publishing handles' counters, in their order.
pub(in crate::metrics::export) const PUBLISHER: [PublisherFamily; PUBLISHER_COUNT] = [
    PublisherFamily {
        name: "gsb_publisher_events_pushed_total",
        help: "Pushes the event router's mailbox took from this publisher, cumulative.",
        get: |c| c.events_pushed,
    },
    PublisherFamily {
        name: "gsb_publisher_events_refused_router_full_total",
        help: "Pushes refused at the handle: the event router's mailbox was full, cumulative.",
        get: |c| c.events_refused_router_full,
    },
    PublisherFamily {
        name: "gsb_publisher_events_refused_router_gone_total",
        help: "Pushes refused at the handle: the event router was gone, cumulative.",
        get: |c| c.events_refused_router_gone,
    },
    PublisherFamily {
        name: "gsb_publisher_events_refused_too_large_total",
        help: "Pushes refused at the handle: the encoded event exceeded max_event_bytes, cumulative.",
        get: |c| c.events_refused_too_large,
    },
];

#[cfg(test)]
mod tests;
