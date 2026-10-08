//! The event path's counters (`docs/EVENTS.md` §9, round e2): every leg
//! a push could not deliver is counted under its own name (B32, "count
//! every loss"), at the place that lost it.
//!
//! - [`PublisherCounts`] — the publishing handle's own, per publisher
//!   name (atomics on the handle, read by the collector at each report,
//!   the service-stats pattern): what the router's mailbox took and the
//!   typed refusals the handle returned.
//! - [`RouterCounts`] — the registry's routing table
//!   ([`EventRoutes`](crate::events)): what became of every recipient and
//!   every leg.
//! - [`PushCounts`] — a connection actor's PUSH frames (a delta, on the
//!   connection's sample).
//! - [`EventCounts`] — a room's or shard's event table and its EVENTS
//!   phase.
//!
//! Ledgers (identity-addressed pushes; `docs/EVENTS.md` §9):
//!
//! - per recipient, `offline` **or** one full ending per leg it has:
//!   CLIENT `client_{sent, parked, unsupported, refused_conn_full,
//!   refused_conn_gone}`, WORLD `world_{sent, no_room,
//!   refused_room_full, refused_room_gone}`;
//! - `client_sent = pushes_written + pushes_dropped_outbound_full +
//!   pushes_dropped_outbound_gone + pushes_dropped_unread + (in flight)`;
//! - `world_sent + forwarded = admitted + no_handler + undecodable +
//!   unbound + world_not_here + forward_refused + fence_overflow + (held,
//!   in flight)`, the event counts summed over a room's actors (a
//!   forwarded leg is ended once more, by the shard it was handed to);
//!   `admitted = taken + untaken`.

/// Declares a counter set and its whole-set operations (the
/// [`CallCounts`](crate::metrics::CallCounts) pattern: a field added to
/// the list cannot be left out of one of them).
macro_rules! event_counts {
    ($(#[$sdoc:meta])* $name:ident, $count:ident { $( $(#[$doc:meta])* $field:ident, )* }) => {
        $(#[$sdoc])*
        #[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
        pub struct $name {
            $( $(#[$doc])* pub $field: u64, )*
        }

        impl $name {
            /// Add another set (a fold over rows or deltas).
            pub fn add(&mut self, o: &Self) {
                $( self.$field = self.$field.saturating_add(o.$field); )*
            }

            /// Every field as `(name, value)`, in declaration order (the
            /// log line's keys, the RESULT keys, the loadgen wire's
            /// order).
            pub fn fields(&self) -> [(&'static str, u64); $count] {
                [ $( (stringify!($field), self.$field), )* ]
            }

            /// The inverse of [`Self::fields`]' values (the loadgen wire).
            pub fn from_values(v: [u64; $count]) -> Self {
                let mut i = 0;
                $( let $field = v[i]; i += 1; )*
                let _ = i;
                Self { $( $field, )* }
            }

            /// Whether every counter is zero.
            pub fn is_zero(&self) -> bool {
                *self == Self::default()
            }
        }

        #[doc = concat!("How many counters [`", stringify!($name), "`] has.")]
        pub const $count: usize = [$( stringify!($field), )*].len();
    };
}

event_counts! {
    /// A room's (or shard's) event table and EVENTS phase (see the module
    /// docs). One type in the table, the room's sample and its report row.
    EventCounts, EVENT_COUNT {
        /// WORLD legs that entered the table (decoded, owner resolved).
        events_admitted,
        /// WORLD legs of an op this actor never `accept`ed: dropped.
        events_no_handler,
        /// WORLD legs whose body did not decode as the accepted type:
        /// dropped.
        events_undecodable,
        /// WORLD legs for a member the game bound to no owner
        /// (`GameLogic::call_owner` answered `None`): dropped.
        events_unbound,
        /// Events the game took (`take`, `take_all`).
        events_taken,
        /// Events nobody took by tick end: dropped (never carried into
        /// the next tick).
        events_untaken,
        /// WORLD legs for an identity that is no member of this actor
        /// and that it keeps no tombstone for (it left the room, or a
        /// migration's tombstone expired, between routing and this
        /// EVENTS): dropped.
        events_world_not_here,
        /// WORLD legs a shard handed on to the shard its member migrated
        /// to (the tombstone, `docs/EVENTS.md` §7.3) — a transfer, not an
        /// ending: the receiving shard ends the leg.
        events_forwarded,
        /// WORLD legs a tombstone could not hand on (the target shard's
        /// link full or closed): dropped.
        events_forward_refused,
        /// Direct WORLD legs held behind a migration's fence (or, on a
        /// shard the member left again before its incoming fence, kept
        /// pending for it) — a wait, not an ending.
        events_fence_held,
        /// Held WORLD legs a fence (or its timeout) let go, in order.
        events_fence_released,
        /// Direct WORLD legs dropped because a fence's hold was full
        /// (`HOLD_CAP`).
        events_fence_overflow,
        /// Fences that never came within `FENCE_TTL` ticks: the held legs
        /// were released anyway — the per-publisher order is not
        /// guaranteed for them.
        events_fence_timeouts,
        /// Seals and fences nobody waited for (an expired tombstone or
        /// hold, a refused `Relocated`, a stale migration count).
        events_fence_lost,
        /// `Relocated` reports the registry's full or closed mailbox
        /// refused: the shard opens no hold; the source's tombstone
        /// forwards until it expires.
        events_relocations_refused,
        /// Tombstones that expired (`TOMB_TTL` ticks); a later leg for
        /// that identity is `events_world_not_here`.
        events_tombstones_expired,
    }
}

event_counts! {
    /// The registry's routing table (see the module docs).
    RouterCounts, ROUTER_COUNT {
        /// Pushes the router took from its mailbox.
        events_routed,
        /// Pushes still in the router's mailbox when it stopped: never
        /// routed (`events_pushed`, summed over the publishers, =
        /// `events_routed + events_unread` + in flight).
        events_unread,
        /// Identity-addressed recipients with no row (no authenticated
        /// session, nothing parked): counted once per recipient.
        events_offline,
        /// CLIENT legs handed to a connection actor.
        events_client_sent,
        /// CLIENT legs for a parked identity (no live session; the
        /// number is spent, the client sees the gap).
        events_client_parked,
        /// CLIENT legs for a session whose client did not announce
        /// `FEATURE_PUSH`.
        events_client_unsupported,
        /// CLIENT legs the connection actor's full inbox refused.
        events_client_refused_conn_full,
        /// CLIENT legs whose connection actor was gone.
        events_client_refused_conn_gone,
        /// WORLD legs handed to a room's or shard's event mailbox.
        events_world_sent,
        /// WORLD legs for an identity in no room (or a room-wide leg for
        /// a room the router does not know).
        events_world_no_room,
        /// WORLD legs the room's full event mailbox refused.
        events_world_refused_room_full,
        /// WORLD legs whose room had ended (its event mailbox closed).
        events_world_refused_room_gone,
        /// Shard migrations a shard reported (`Relocated`) that moved an
        /// identity's WORLD legs to its new shard.
        events_relocations,
        /// `Relocated` reports the router did not apply: an older (or the
        /// same) migration count than it knows, a room incarnation that
        /// ended, or an identity with no membership there.
        events_relocations_stale,
        /// Seals sent to a migration's source shard (the fence,
        /// `docs/EVENTS.md` §7.3).
        events_seals_sent,
        /// Seals the source shard's full or closed event mailbox refused:
        /// the target's hold times out (`events_fence_timeouts`).
        events_seals_refused,
    }
}

event_counts! {
    /// A connection actor's PUSH frames (a delta on its sample; see the
    /// module docs).
    PushCounts, PUSH_COUNT {
        /// PUSH frames queued on the connection's outbound channel.
        pushes_written,
        /// PUSH frames the full outbound channel refused (a slow client).
        pushes_dropped_outbound_full,
        /// PUSH frames for a connection whose writer was gone.
        pushes_dropped_outbound_gone,
        /// CLIENT legs still in the connection actor's inbox when its
        /// session ended: never written.
        pushes_dropped_unread,
    }
}

event_counts! {
    /// One publishing handle's counters (all clones of it share them).
    PublisherCounts, PUBLISHER_COUNT {
        /// Pushes the router's mailbox took.
        events_pushed,
        /// Pushes refused: the router's mailbox was full
        /// (`PushRefused::RouterFull`).
        events_refused_router_full,
        /// Pushes refused: the router was gone (`PushRefused::RouterGone`).
        events_refused_router_gone,
        /// Pushes refused: the encoded body exceeded the handle's
        /// `max_event_bytes` (`PushRefused::TooLarge`).
        events_refused_too_large,
    }
}

/// One publisher's slice of a report.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PublisherReport {
    /// The publisher's name (`RegistryParts::publisher(name)`).
    pub publisher: String,
    /// Its id (`PublisherId`, the PUSH frame's `publisher`).
    pub id: u32,
    /// Its counters (cumulative).
    pub counts: PublisherCounts,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_wire_order_round_trips_every_set() {
        let mut v = [0u64; EVENT_COUNT];
        for (i, x) in v.iter_mut().enumerate() {
            *x = i as u64 + 1;
        }
        let c = EventCounts::from_values(v);
        assert_eq!(c.fields().map(|f| f.1), v);
        let mut sum = c;
        sum.add(&c);
        assert_eq!(sum.events_world_not_here, 2 * EVENT_COUNT as u64);
        assert!(!c.is_zero() && EventCounts::default().is_zero());
        let r = RouterCounts::from_values([7; ROUTER_COUNT]);
        assert!(r.fields().iter().all(|f| f.1 == 7));
        let p = PushCounts::from_values([3; PUSH_COUNT]);
        assert_eq!(p.pushes_dropped_unread, 3);
        let q = PublisherCounts::from_values([1, 2, 3, 4]);
        assert_eq!(q.events_refused_too_large, 4);
    }
}
