//! Ordered teardown: drain the dispatchers, stop the ticker, drop the
//! tables. Every task the registry started exits because the channel
//! it reads closes — none is cancelled.

use crate::channel::Mailbox;
use crate::conn::ConnIn;
use crate::registry::actor::Registry;
use crate::registry::*;
use crate::room::RoomControl;
use crate::shard::ShardMsg;
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
    /// The `Shutdown` arm of [`Self::run`].
    pub(super) async fn on_shutdown(&mut self) {
        warn!("registry shutting down");
        // 1. Ask every dispatcher to drain (final leaves for
        //    in-flight joins), then drop the senders so they
        //    exit after draining.
        for op_tx in self.conn_ops.values() {
            let _ = op_tx.try_send(RoomOp::Close);
        }
        self.conn_ops.clear();
        // 2. Notify every registered connection.
        let doomed: Vec<Mailbox<ConnIn>> = self
            .conns
            .values()
            .filter_map(|i| i.inbox.clone())
            .collect();
        for inbox in doomed {
            tokio::spawn(async move {
                let _ = inbox.send(ConnIn::Shutdown).await;
            });
        }
        self.conns.clear();
        // 3. Stop every room (a single room via its control
        //    channel; a sharded room via one Shutdown per
        //    shard) — processed on the next tick; the
        //    composition root aborts the ticker afterwards,
        //    which closes the broadcast as a backstop for any
        //    room that misses the window.
        // `std::mem::take` rather than `drain()`: the loop
        // body awaits, and a live drain borrow would fight
        // the awaited sends' executor hops in future edits.
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
            match (entry.control, entry.shards) {
                (Some(control), _) => {
                    let _ = control.send(RoomControl::Shutdown).await;
                }
                (None, Some(group)) => {
                    for tx in &group.mailboxes {
                        let _ = tx.send(ShardMsg::Shutdown).await;
                    }
                }
                (None, None) => {}
            }
            debug!(room = %id, "room stopped");
        }
        // 4. Stop the actor now. (It cannot wait for the mailbox
        //    to close: it holds a clone of it — `self_mailbox` —
        //    for dispatcher reporting, so EOF would never come.)
        //    Dispatchers already received Close and will exit on
        //    their own; their stray reports fail against the
        //    dropped inbox, harmlessly. The death watchers exit
        //    the same way: when the ticker abort closes each
        //    room's tick channel, every watcher's report fails
        //    against our dropped mailbox and the watcher stops
        //    (no task leaks beyond the server's lifetime).
    }
}
