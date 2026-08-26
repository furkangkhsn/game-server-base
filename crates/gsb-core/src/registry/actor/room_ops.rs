//! Room lifecycle messages: create (idempotent, un-retires), destroy
//! (entry removed first), and the death report a watcher files.

use std::fmt::Debug;
use std::hash::Hash;

use tokio::sync::oneshot;
use tracing::{debug, warn};

use crate::error::CoreError;
use crate::id::RoomId;
use crate::room::{RoomConfig, RoomControl};
use crate::shard::ShardMsg;
use crate::registry::*;

use crate::registry::actor::Registry;

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
    pub(super) async fn on_create_room(
        &mut self,
        config: RoomConfig,
        reply: oneshot::Sender<Result<RoomStatus, CoreError>>,
    ) {
        let id = config.id;
        // A retired id stays closed to ACCIDENTAL traffic
        // (joins/resumes answer ERROR 12 while absent), but an
        // EXPLICIT create is the operator revisiting its ending
        // decision — un-retire it and proceed (§8 protects
        // against AUTOMATIC resurrection, e.g. the panic
        // rebuild, never against a deliberate new create).
        if self.retired.contains_key(&id) {
            self.unretire_room(&id);
            warn!(
                room = %id,
                "re-creating a RETIRED room id (explicit override; \
                 joins answer normally from here on)"
            );
        }
        // Idempotency: a room that already exists is a no-op
        // for an IDENTICAL request (the control plane's retry
        // pattern) and a conflict for a different one. The
        // whole-config comparison is the "same request"
        // definition (see the message docs).
        if let Some(existing) = self.rooms.get(&id) {
            if existing.config == config {
                let members = self.room_members(id);
                debug!(room = %id, "room create idempotent (already exists)");
                let _ = reply.send(Ok(RoomStatus::Running { members }));
            } else {
                warn!(room = %id, "room create conflict: different config");
                let _ = reply.send(Err(CoreError::RoomConflict(id.0)));
            }
            return;
        }
        // The room rate must divide the global ticker rate: the
        // room steps on every k-th global tick (k = run_every).
        let global = self.ticker.hz();
        let run_every = (global / config.tick_hz).round() as u64;
        if run_every < 1 || (global - config.tick_hz * run_every as f64).abs() > 1e-3 {
            let _ = reply.send(Err(CoreError::TickRate {
                room: config.tick_hz,
                global,
            }));
            return;
        }
        // Keep-alive cannot run faster than the room's own tick:
        // the cadence would clamp to every step, the "silence
        // when unchanged" gain would be lost, and clients would
        // receive fewer keep-alives than configured. Reject the
        // config rather than start a silently degraded room.
        if config.keepalive_hz > 0.0 && config.keepalive_hz > config.tick_hz {
            let _ = reply.send(Err(CoreError::KeepaliveRate {
                keepalive: config.keepalive_hz,
                tick: config.tick_hz,
            }));
            return;
        }
        let built = (self.factory)(id, &config);
        // The factory runs inside the registry loop on purpose
        // (unchanged): room construction is synchronous game
        // code, and a panicking FACTORY is out of scope for the
        // death watch — it kills the registry itself, exactly
        // as it does today. Supervision covers the spawned
        // actor tasks (the tick loops), not this call.
        //
        // `install_room` is THE single creation path: both the
        // create here and a panic rebuild (see
        // `RegistryMsg::RoomDied`) wire the actors identically
        // through it — one implementation, no drift.
        self.install_room(config.clone(), built, run_every);
        self.reg_created += 1;
        self.emit_metrics();
        debug!(room = %id, "room created");
        // The reply is the status (a create round trip doubles
        // as a status query — one hop, not two); a fresh room
        // starts with zero members.
        let _ = reply.send(Ok(RoomStatus::Running { members: 0 }));
    }

    pub(super) async fn on_destroy_room(
        &mut self,
        id: RoomId,
        reply: oneshot::Sender<RoomStatus>,
    ) {
        if let Some(entry) = self.rooms.remove(&id) {
            // The entry is gone FIRST (synchronously) — this is
            // also what makes the death watcher's late report
            // for this room a silent no-op (see
            // `RegistryMsg::RoomDied`).
            //
            // Members learn `RoomGone`, affiliations clear —
            // exactly the semantics an unexpected death gets
            // below (same helper on purpose: a member must not
            // be able to tell how the room ended).
            self.notify_room_gone(id);
            // The room processes it on its next tick (the ticker
            // is still running); aborting the ticker later closes
            // its broadcast as a backstop.
            match (entry.control, entry.shards) {
                (Some(control), _) => {
                    let _ = control.send(RoomControl::Shutdown).await;
                }
                (None, Some(group)) => {
                    // Sharded: one Shutdown per shard. Bounded
                    // sends (the same idiom as the single room);
                    // a stalled shard parks the send briefly and
                    // the ticker abort remains the backstop.
                    for tx in &group.mailboxes {
                        let _ = tx.send(ShardMsg::Shutdown).await;
                    }
                }
                (None, None) => {}
            }
            self.reg_destroyed += 1;
            self.emit_room_gone(id);
            self.emit_metrics();
            // The id is retired (§8): an ephemeral match ended,
            // or a persistent room was decommissioned — either
            // way later joins/resumes answer ERROR 12, and a
            // create cannot silently reopen it.
            self.retire_room(id);
            debug!(room = %id, "room destroyed (id retired)");
            let _ = reply.send(RoomStatus::Destroyed);
        } else {
            // Idempotent destroy: a missing room is a no-op
            // success (a control plane retry never errors on
            // the second attempt).
            debug!(room = %id, "room destroy: absent (idempotent no-op)");
            let _ = reply.send(RoomStatus::Absent);
        }
    }

    pub(super) async fn on_room_died(
        &mut self,
        id: RoomId,
        shard: Option<usize>,
        generation: u64,
    ) {
        // The design trick (no cancellation plumbing): a NORMAL
        // end — `DestroyRoom`, server `Shutdown` — removes the
        // table entry first, so this report finds either no
        // entry, or an entry of a DIFFERENT incarnation (the id
        // was re-created in the meantime), and must stay
        // silent. Only a live entry of the SAME generation is an
        // unexpected death.
        let is_current = self
            .rooms
            .get(&id)
            .map(|e| e.generation == generation)
            .unwrap_or(false);
        if !is_current {
            debug!(
                room = %id,
                shard = ?shard,
                "late death report for a destroyed/replaced room; ignored"
            );
            return;
        }
        // Unexpected death. For a sharded room, ANY dead shard
        // means the whole LOGICAL room is broken — its neighbors
        // hold senders into the dead shard's closed mailbox, so
        // cross-shard migration can never complete again. There
        // is no partial-shard recovery: the whole room goes,
        // exactly like a single-room death.
        warn!(
            room = %id,
            shard = ?shard,
            "room task died unexpectedly (panic in game logic?); \
             reaping the room"
        );
        let entry = self.rooms.remove(&id).expect("generation checked above");
        // Exactly the destroy semantics: members learn
        // `ConnIn::RoomGone`, affiliations clear, status turns
        // `Absent`.
        self.notify_room_gone(id);
        self.reg_died += 1;
        self.emit_room_gone(id);
        self.emit_metrics();
        // Restart policy: `restart_on_panic` is the operator's
        // preference, but a PERSISTENT room's class guarantee
        // overrides it (§8 — "sürekli oda panikle ölü kalmaz"
        // is a class property, not a tunable): the rebuild is
        // forced ON. The rebuilt room comes back EMPTY either
        // way — the members were notified above and may
        // rejoin; no world state survives (it lived inside the
        // dead task). A logic that panics persistently yields
        // a restart-per-death cycle, one warn per round:
        // immediately visible to the operator, deemed
        // acceptable for v1 (no backoff machinery).
        //
        // In-flight joins dispatched against the dead
        // incarnation settle later (SpawnDone/SpawnFailed);
        // their sharded-counter effects land on the NEW
        // ShardGroup with saturating arithmetic — bounded
        // imprecision (at most the number of in-flight joins),
        // never a panic or a permanently stuck cap.
        if entry.config.restart_on_panic || entry.config.persistent {
            warn!(
                room = %id,
                persistent = entry.config.persistent,
                "rebuilding the room from its factory + config \
                 (it comes back EMPTY)"
            );
            let global = self.ticker.hz();
            // The config was validated when this room was first
            // created (tick rate divides the global rate), so
            // the recomputed step divisor is >= 1 by
            // construction.
            let run_every = (global / entry.config.tick_hz).round() as u64;
            let built = (self.factory)(id, &entry.config);
            self.install_room(entry.config.clone(), built, run_every);
            debug!(room = %id, "room rebuilt after unexpected death");
        } else {
            // A death left unrebuilt ends the match: retire the
            // id so later joins answer ERROR 12 ("definitively
            // over") instead of 4 ("unknown/temporary").
            self.retire_room(id);
        }
    }
}
