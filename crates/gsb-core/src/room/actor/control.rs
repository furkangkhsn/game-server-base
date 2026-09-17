//! Phase 0 — CONTROL: joins, leaves, resumes and shutdown, plus the
//! roster bookkeeping every one of them touches.

use crate::error::CoreError;
use crate::id::PlayerId;
use crate::room::*;
use std::fmt::Debug;
use std::hash::Hash;
use std::time::Instant;
use tracing::{debug, warn};

use crate::room::actor::RoomActor;

mod join;

impl<W, G, Sp> RoomActor<W, G, Sp>
where
    G: Eq + Hash + Clone + Debug,
    // The strip payload's trait bounds (`GameLogic::Strip`) — restated so
    // calls through the logic object type-check; the room itself never
    // touches the payloads.
    Sp: Debug + Clone + PartialEq + Send + 'static,
{
    /// Register a freshly joined player at the tail of the READ
    /// roster (why the roster exists: see the field docs and the READ
    /// phase's rotation note).
    pub(super) fn roster_add(&mut self, player: PlayerId) {
        self.roster_pos.insert(player, self.roster.len());
        self.roster.push(player);
    }

    /// Remove a player from the READ roster: swap-remove plus one
    /// index fix for the element moved into the freed slot (O(1)
    /// amortized; no scan). Called exactly where `conns` loses an entry
    /// (a leave, or a join superseding a stale session), so the roster
    /// cannot drift from the table.
    ///
    /// The fix targets the element that RELOCATED — `swap_remove`
    /// returns the REMOVED element (`player` itself), and the former last
    /// element lands at `idx`. Missing that distinction silently left
    /// the relocated element's position stale (the mass-leave panic the
    /// supervision round surfaced).
    pub(super) fn roster_remove(&mut self, player: &PlayerId) {
        if let Some(idx) = self.roster_pos.remove(player) {
            let relocated = *self
                .roster
                .last()
                .expect("pos entry implies non-empty roster");
            self.roster.swap_remove(idx);
            if relocated != *player {
                // `insert`, not `get_mut`: the relocated element's entry
                // exists while the invariant holds, and rewriting it
                // keeps this function correct even under partial drift.
                self.roster_pos.insert(relocated, idx);
            }
        }
    }

    pub(in crate::room) fn handle_control(&mut self, c: RoomControl) -> bool {
        match c {
            RoomControl::Join { conn, out, reply } => self.admit_fresh(conn, out, reply),
            RoomControl::Leave { conn, entity } => {
                // Stale-leave guard: resolve the session through the
                // binding, then only the entity this player currently
                // owns. An unbound conn (a shard/room that never knew it,
                // or an already-torn-down session) is a no-op.
                if let Some(&player) = self.binding.get(&conn)
                    && self.conns.get(&player).map(|c| c.entity) == Some(entity)
                {
                    self.despawn_conn(player, true);
                    debug!(room = %self.config.id, %conn, %player, entity, "player left");
                }
                true
            }
            RoomControl::Detach {
                conn,
                entity,
                identity,
            } => {
                // Transport death (registry `ConnClosed` route): the POLICY
                // is the logic's (§3 — the registry only reports the fact).
                // Same stale guard as `Leave`: binding first, then the
                // entity this player currently owns.
                if let Some(&player) = self.binding.get(&conn)
                    && self.conns.get(&player).map(|c| c.entity) == Some(entity)
                {
                    let decision = self.logic.on_disconnect(&mut self.world, player, &identity);
                    match decision {
                        Detach::Despawn => {
                            // Byte-for-byte the old close semantics.
                            self.despawn_conn(player, false);
                        }
                        Detach::Hold { grace, to } => {
                            // Park it: keep the row (under its STABLE
                            // player key — nothing is re-keyed), the
                            // entity, the world state, the group membership
                            // AND the cap slot (§4 — members accounting
                            // does not drop). The binding row stays too:
                            // the parked row still belongs to that (dead)
                            // session until a resume re-points it. The
                            // clock is CORE-owned (§14.4): the grace is
                            // written here as an absolute deadline and the
                            // CONTROL sweep below fires it.
                            let rc = self.conns.get_mut(&player).expect("guarded above");
                            rc.detached = true;
                            rc.expire_to = to;
                            rc.detach_deadline = grace.map(|g| Instant::now() + g);
                            // Today's leave semantics for in-flight work
                            // (§11 "RPC pending detach anında"): pending
                            // requests drop, late reports are silently
                            // discarded (structural already), queued answers
                            // for the dead session go.
                            self.drop_conn_request_state(conn);
                            debug!(
                                room = %self.config.id,
                                %conn,
                                %player,
                                entity,
                                ?grace,
                                ?to,
                                "player detached (entity parked)"
                            );
                        }
                    }
                }
                true
            }
            RoomControl::Resume {
                conn,
                epoch,
                identity,
                out,
                reply,
            } => {
                // The implicit resume attempt (§14.3): ledger first, fresh
                // join as the transparent fallback (§5). An empty identity
                // never resumes (nothing to look up; the local-auth demo
                // may still send names, an anonymous client cannot).
                if identity.is_empty() {
                    return self.admit_fresh(conn, out, reply);
                }
                match self.logic.resume_lookup(&self.world, &identity) {
                    ResumeFound::Held(player) => {
                        // The parked row, by its STABLE key: one lookup
                        // (the ledger rides the player state, §14.2, so it
                        // answers with the id the table is keyed by — the
                        // pre-Faz-2 linear scan over the detached subset
                        // is gone).
                        let Some(rc) = self.conns.get(&player) else {
                            // Ledger says held but the table lost the row
                            // (a logic bug, or the hold expired in this
                            // very tick's sweep above): treat as ended.
                            self.m.resume_rejected_stale += 1;
                            debug!(
                                room = %self.config.id,
                                %identity,
                                %player,
                                "resume rejected: ledger holds a row the table lost"
                            );
                            return self.admit_fresh(conn, out, reply);
                        };
                        if !rc.detached {
                            // The player's row is LIVE (a double session of
                            // an identity the ledger somehow still holds):
                            // same divergence posture as above.
                            self.m.resume_rejected_stale += 1;
                            debug!(
                                room = %self.config.id,
                                %identity,
                                %player,
                                "resume rejected: ledger holds a live row"
                            );
                            return self.admit_fresh(conn, out, reply);
                        }
                        // Epoch guard (§7): one integer comparison rejects a
                        // delayed duplicate/replay AFTER a newer session
                        // already took the park over. Without it the loser of
                        // two racing resumes could fresh-join a SECOND entity
                        // for one identity. `0` disables the guard (hand-built
                        // calls); the ledger consumption itself stays
                        // exactly-once regardless — this actor is
                        // single-threaded.
                        if epoch != 0 && rc.session_epoch != 0 && epoch <= rc.session_epoch {
                            self.m.resume_rejected_stale += 1;
                            warn!(
                                room = %self.config.id,
                                %conn,
                                %player,
                                %identity,
                                resume_epoch = epoch,
                                "resume rejected: stale epoch (a newer session \
                                 already rebound this park)"
                            );
                            let _ = reply.send(Err(CoreError::ResumeStale));
                            return true;
                        }
                        self.rebind_session(player, conn, epoch, identity, out, reply);
                        true
                    }
                    ResumeFound::Ended => {
                        // The hold already ended (expired/consumed/
                        // superseded): the RESUME mechanism rejects (counted),
                        // while the client-visible outcome stays the
                        // transparent fresh join of §5 — no waiting endpoint,
                        // no error frame; the TOCTOU rule ("whichever branch
                        // lands first wins, both are valid") covers exactly
                        // this race.
                        self.m.resume_rejected_stale += 1;
                        debug!(
                            room = %self.config.id,
                            %identity,
                            "resume rejected stale (hold ended); falling back \
                             to a fresh join"
                        );
                        self.admit_fresh(conn, out, reply)
                    }
                    ResumeFound::Never => self.admit_fresh(conn, out, reply),
                }
            }
            RoomControl::Shutdown => false,
        }
    }
}
