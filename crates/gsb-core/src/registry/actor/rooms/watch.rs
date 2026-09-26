//! The death watch: one task per spawned room/shard, and the
//! notification members get when their room ends by any route.

use crate::channel::Mailbox;
use crate::conn::ConnIn;
use crate::id::{ConnectionId, RoomId};
use crate::registry::actor::Registry;
use crate::registry::*;
use crate::service::Hold;
use std::fmt::Debug;
use std::hash::Hash;
use tracing::debug;

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
    /// Give the registry the rooms' drop barrier token (BACKLOG F5, see
    /// [`crate::service`]): the registry keeps it until it exits and hands
    /// a clone to every room's (and shard's) death watcher, which drops it
    /// when that task has ended — after its teardown hooks
    /// (`on_shutdown`, `match_result`). The barrier's waiter therefore
    /// completes once the registry is gone and every room it ever spawned
    /// has finished, and nothing ever awaits a room to learn it. Without
    /// it (the default) nothing changes.
    pub fn with_rooms_hold(mut self, hold: Hold) -> Self {
        self.rooms_hold = Some(hold);
        self
    }

    /// One watcher task per spawned room/shard task. It awaits ONLY that
    /// task's `JoinHandle` — the project's "one watcher per source, the
    /// owner awaits a single receive" idiom (see the signal handling in
    /// `main.rs`, the conn_ops dispatchers) — and reports the exit through
    /// the registry's own mailbox, exactly like the dispatchers report
    /// their completions.
    ///
    /// No cancellation plumbing, deliberately: EVERY exit reports, because
    /// `DestroyRoom` and server `Shutdown` also end room tasks normally.
    /// The discrimination happens on the receiving side instead (the design
    /// trick): a destroy removes the table entry FIRST and synchronously,
    /// so its late report finds no entry — or, if the id was re-created in
    /// between, an entry of a different `generation`. Either way the report
    /// is a silent no-op. Only a live entry with a matching generation is
    /// an *unexpected* death.
    ///
    /// The panic payload itself is not carried here: it is printed by the
    /// default panic hook when the task unwinds; what the base adds is the
    /// operator-facing attribution (which room, which shard, what happened
    /// to the members).
    ///
    /// The watcher also carries the rooms' drop barrier token, if any (see
    /// [`Self::with_rooms_hold`]), and drops it the moment the room task
    /// has ended — before reporting, so a registry that is already gone
    /// cannot delay the release.
    pub(in crate::registry) fn spawn_room_watcher(
        id: RoomId,
        shard: Option<usize>,
        generation: u64,
        handle: tokio::task::JoinHandle<()>,
        registry: Mailbox<RegistryMsg>,
        hold: Option<Hold>,
    ) {
        tokio::spawn(async move {
            // The outcome (panic vs clean return) is deliberately not
            // inspected: the registry decides whether this exit means
            // anything, based on its table state at report time.
            let _ = handle.await;
            drop(hold);
            let _ = registry
                .send(RegistryMsg::RoomDied {
                    id,
                    shard,
                    generation,
                })
                .await;
        });
    }

    /// Notify every connection affiliated with `room` that the room is
    /// gone ([`ConnIn::RoomGone`], fire-and-forget spawned sends) and clear
    /// their affiliations; their inbox is kept (clone, don't take) so they
    /// can still receive `Shutdown` or later notifications.
    ///
    /// Shared by the destroy path and the unexpected-death path (see
    /// `RegistryMsg::RoomDied`) on purpose: members must not be able to
    /// tell how the room ended.
    pub(in crate::registry) fn notify_room_gone(&mut self, room: RoomId) {
        let mut doomed = Vec::new();
        let mut dead_detached: Vec<ConnectionId> = Vec::new();
        for (conn, info) in self.conns.iter_mut() {
            if info.room == Some(room) {
                info.room = None;
                info.entity = None;
                if info.detached {
                    // A parked player's socket is already gone: no
                    // notification can reach anyone and the affiliation is
                    // over — the entry goes (the room ending released its
                    // slot room-side too).
                    info.detached = false;
                    dead_detached.push(*conn);
                } else if let Some(inbox) = info.inbox.clone() {
                    doomed.push((*conn, inbox));
                }
            }
        }
        for conn in dead_detached {
            self.conns.remove(&conn);
        }
        for (conn, inbox) in doomed {
            // Fire-and-forget notification (no reply needed).
            tokio::spawn(async move {
                let _ = inbox.send(ConnIn::RoomGone(room)).await;
                debug!(%conn, room = %room, "notified: room gone");
            });
        }
    }
}
