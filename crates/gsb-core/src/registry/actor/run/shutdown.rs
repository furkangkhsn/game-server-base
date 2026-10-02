//! Ordered teardown: drain the dispatchers, notify the connections, stop
//! the rooms without awaiting them, drop the tables. Every task the
//! registry started exits because the channel it reads closes — none is
//! cancelled.

use crate::channel::Mailbox;
use crate::conn::ConnIn;
use crate::registry::actor::Registry;
use crate::registry::*;
use std::fmt::Debug;
use std::hash::Hash;
use tracing::{debug, warn};

impl<W, G, St, Sp> Registry<W, G, St, Sp>
where
    W: Send + 'static,
    G: Eq + Hash + Clone + Debug + Send + 'static,
    St: Debug + Send + 'static,
    // The strip payload's trait bounds (`GameLogic::Strip`) — the
    // registry never inspects payloads, but both actor shapes it spawns
    // require them.
    Sp: Debug + Clone + PartialEq + Send + 'static,
{
    /// The `Shutdown` arm of [`Self::run`]. Synchronous by design:
    /// nothing on this path may wait for a room (see the `stop` module),
    /// so the registry always exits and drops its `Ticker`.
    pub(super) fn on_shutdown(&mut self) {
        warn!("registry shutting down");
        // 1. Ask every dispatcher to drain (final leaves for
        //    in-flight joins), then drop the senders so they
        //    exit after draining.
        for (_, op_tx) in self.conn_ops.values() {
            let _ = op_tx.try_send(RoomOp::Close { verdict: None });
        }
        self.conn_ops.clear();
        // 2. Notify every registered connection — naming the verdict
        //    still waiting for room in its inbox, if any: the stop may
        //    overtake it, and the connection that reads the stop first
        //    counts it lost (F60, the `tell` module).
        let doomed: Vec<(Mailbox<ConnIn>, ConnIn)> = self
            .conns
            .values()
            .filter_map(|i| {
                let notice = match i.verdict_in_flight {
                    Some(verdict) => ConnIn::ShutdownOvertaking(verdict),
                    None => ConnIn::Shutdown,
                };
                Some((i.inbox.clone()?, notice))
            })
            .collect();
        for (inbox, notice) in doomed {
            tokio::spawn(async move {
                let _ = inbox.send(notice).await;
            });
        }
        self.conns.clear();
        // 3. Stop every room (a single room via its control
        //    channel; a sharded room via one Shutdown per
        //    shard) WITHOUT awaiting any of them (see the
        //    `stop` module): the composition root aborts the
        //    ticker right after enqueueing our `Shutdown`, so a
        //    room whose channel is full never drains it again —
        //    a bounded send here would park the registry
        //    forever, and with it the `Ticker` whose drop closes
        //    the broadcast (the rooms' global stop signal). A
        //    room that sees neither its `Shutdown` on a tick nor
        //    anything else still exits through `Closed` once we
        //    return and drop the `Ticker`.
        //
        // No `RoomGone` notice here, DELIBERATELY: this is
        // the whole-server shutdown — the collector dies with
        // the ticker right after its final report, so there
        // is no leak to prune; dropping the accumulators
        // first would instead erase every room's LAST report
        // window (consumers read the room line off that final
        // report). The destroy and unexpected-death paths DO
        // notify: they happen mid-flight, where an accumulator
        // would otherwise outlive its room forever.
        for (id, entry) in std::mem::take(&mut self.rooms) {
            Self::stop_room(entry);
            debug!(room = %id, "room stopped");
        }
        // 4. Stop the actor now. (It cannot wait for the mailbox
        //    to close: it holds a clone of it — `self_mailbox` —
        //    for dispatcher reporting, so EOF would never come.
        //    It needs no EOF either: the `Shutdown` arm closed the
        //    inbox and counted what it held before calling this —
        //    F53, the `leftovers` module.) Dispatchers already
        //    received Close and will exit on their own; their stray
        //    reports fail against the closed inbox, harmlessly. The death watchers exit
        //    the same way: when the ticker abort closes each
        //    room's tick channel, every watcher's report fails
        //    against our dropped mailbox and the watcher stops
        //    (no task leaks beyond the server's lifetime).
    }
}
