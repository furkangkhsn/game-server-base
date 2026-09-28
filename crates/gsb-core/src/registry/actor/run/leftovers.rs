//! What the registry's mailbox still holds when it stops (BACKLOG F53):
//! the messages queued behind its `Shutdown`. A child of the run loop.
//!
//! The registry reads nothing after its `Shutdown`, and it holds a clone
//! of its own mailbox (§9.1), so it never waits for the mailbox's EOF.
//! Until F53 it simply dropped the receiver — with every message queued
//! behind the `Shutdown`, which each sender had seen succeed. Now the
//! `Shutdown` arm first CLOSES the inbox (a later send fails at its
//! sender, which counts a refusal where it counts one: a shard's team
//! export is `team_export_drops_closed`, a connection's join
//! `joins_unsent` — F54), then drains it with `try_recv`
//! — finite once closed — and decides by kind, against the tables as
//! they stood at the stop:
//!
//! - a team export of the room's live incarnation: the shard counted it
//!   queued (`team_exports`), the hub never relays it — counted,
//!   `team_exports_unread` (a stale incarnation's or an unknown room's
//!   export is a silent no-op for the hub while it runs, so here too);
//! - a join (`SpawnPlayer`, resume attempts included): never handled; its
//!   reply drops and the connection answers its client `ERROR` "registry
//!   unavailable" — counted, `joins_unread`;
//! - a connection opened behind the stop (`ConnOpened`; the server's own
//!   stop closes its doors first, F41, so only a library caller can get
//!   one here): told to stop like every registered connection — nothing
//!   lost, nothing counted.
//!
//! The rest carries nothing the stop does not do anyway. A transport
//! death (`ConnClosed`) and a client's leave (`DespawnPlayer`): the
//! teardown's `RoomOp::Close` makes every dispatcher detach the
//! membership it holds, and every room stops. A dispatcher's echo
//! (`SpawnDone`, `SpawnFailed`, `LeaveDone`, `DetachDone`, `OpsClosed`),
//! a death watcher's `RoomDied` (a panic is counted by the watcher
//! itself, B67) and `Authed` only update tables the teardown drops. A
//! control-plane request (`CreateRoom`, `DestroyRoom`, `RoomStatus`) is
//! answered by its dropped reply.
//!
//! A room's verdicts (`CloseConn`, `LeaveConn`, `DetachDespawned`) are
//! LOST verdicts (F56, re-deciding B57): the connection gets the stop's
//! `ERROR` 14 instead of the verdict's `ERROR` 9, `server_closes` never
//! books its reason, the row is never settled — counted by kind, the
//! close by its reason, without the tables' guards (a room refusing the
//! same verdict at the closed mailbox cannot apply them either; see
//! `crate::metrics::VerdictsLost`).
//!
//! The counts ride the registry's final sample, which the `Shutdown` arm
//! posts (`crate::channel::post`) before it tears the tables down: past
//! a full metrics channel a spawned sender holds its own clone, so the
//! collector's final report — which waits for every session producer's
//! sender to drop (F35) — cannot be emitted without it. The lost
//! verdicts go the same way, as one `MetricsEvent::VerdictsLost`.

use std::fmt::Debug;
use std::hash::Hash;

use crate::conn::ConnIn;
use crate::metrics::{MetricsEvent, VerdictsLost};
use crate::registry::actor::Registry;
use crate::registry::*;

impl<W, G, St, Sp> Registry<W, G, St, Sp>
where
    W: Send + 'static,
    G: Eq + Hash + Clone + Debug + Send + 'static,
    St: Debug + Send + 'static,
    Sp: Debug + Clone + PartialEq + Send + 'static,
{
    /// Close the inbox and count what it still held, by kind (see the
    /// module docs). Synchronous: `try_recv` until the closed queue is
    /// empty. (A sender caught between taking its slot and writing its
    /// message — within one poll on another worker thread — finds the
    /// receiver dropped a moment later, as a room's or shard's stop
    /// drain does, B68.)
    pub(super) fn count_leftovers(&mut self) {
        self.inbox.close();
        let mut lost = VerdictsLost::default();
        while let Ok(msg) = self.inbox.try_recv() {
            match msg {
                RegistryMsg::TeamExport {
                    room, generation, ..
                } => {
                    let relayed = self
                        .rooms
                        .get(&room)
                        .is_some_and(|e| e.generation == generation && e.shards.is_some());
                    if relayed {
                        self.reg_team_exports_unread += 1;
                    }
                }
                RegistryMsg::SpawnPlayer { .. } => self.reg_joins_unread += 1,
                RegistryMsg::ConnOpened { inbox, .. } => {
                    tokio::spawn(async move {
                        let _ = inbox.send(ConnIn::Shutdown).await;
                    });
                }
                RegistryMsg::CloseConn(req) => lost.close(req.cause),
                RegistryMsg::LeaveConn(_) => lost.leaves += 1,
                RegistryMsg::DetachDespawned { .. } => lost.detach_despawns += 1,
                // No match-all arm: a new message kind must be decided
                // here.
                RegistryMsg::CreateRoom { .. }
                | RegistryMsg::DestroyRoom { .. }
                | RegistryMsg::RoomStatus { .. }
                | RegistryMsg::DespawnPlayer { .. }
                | RegistryMsg::ConnClosed { .. }
                | RegistryMsg::Authed { .. }
                | RegistryMsg::Shutdown
                | RegistryMsg::SpawnDone { .. }
                | RegistryMsg::SpawnFailed { .. }
                | RegistryMsg::LeaveDone { .. }
                | RegistryMsg::DetachDone { .. }
                | RegistryMsg::OpsClosed { .. }
                | RegistryMsg::RoomDied { .. } => {}
            }
        }
        crate::room::send_verdicts_lost(&self.metrics, &lost);
    }

    /// The registry's last sample, carrying [`Self::count_leftovers`]'
    /// counts: posted, not `try_send`, so a full metrics channel delays
    /// it instead of losing it (see the module docs).
    pub(super) fn post_final_sample(&self) {
        crate::channel::post(&self.metrics, MetricsEvent::Registry(self.sample()));
    }
}

#[cfg(test)]
mod tests;
