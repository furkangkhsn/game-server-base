//! Phase 0 — CONTROL: joins, leaves, resumes and shutdown, plus the
//! roster bookkeeping every one of them touches.

use crate::error::CoreError;
use crate::id::PlayerId;
use crate::room::*;
use std::fmt::Debug;
use std::hash::Hash;
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

    /// THE disconnect path (`docs/RECONNECT.md` §3): ask the policy what
    /// happens to this member's entity, then run the arm it chose.
    ///
    /// Two callers, ONE decision point — that is the whole reason this is
    /// a function: the registry's `RoomControl::Detach` (a transport that
    /// actually died) and the input-idle ceiling
    /// ([`RoomConfig::max_idle_input_secs`], a transport that is alive but
    /// has stopped playing). The ceiling deliberately does not invent a
    /// second fate for an entity: it hands the member to this path and the
    /// GAME decides park / AI handover / despawn, which is also what gives
    /// a MOBA bot-takeover-on-AFK for nothing.
    ///
    /// Callers own the guards (binding + entity + "not already parked").
    /// `report` = queue the detach-despawn report on the despawn arm (a
    /// caller that settles the registry row by other means — the
    /// ceiling's leave request, B40 — passes `false`).
    pub(in crate::room) fn detach_player(
        &mut self,
        player: PlayerId,
        conn: ConnectionId,
        identity: &str,
        report: bool,
    ) {
        match self.logic.on_disconnect(&mut self.world, player, identity) {
            Detach::Despawn => {
                // Byte-for-byte the old close semantics — plus the report
                // the registry is waiting on.
                //
                // The registry marked this connection's row `detached` and
                // KEPT it (slot held, §4) the moment the transport died,
                // before the policy had answered. A park that never starts
                // has no hold and no deadline, so the phase-0c sweep can
                // never fire for it: this arm is the ONLY place that learns
                // the row is dead. Queued only when there IS a registry — a
                // standalone room has no reader, so the queue must not
                // accumulate. The flush is phase 0c, in this same tick
                // (CONTROL runs first).
                if report && self.registry.is_some() {
                    self.despawn_reports.push(conn);
                }
                self.despawn_conn(player, false);
            }
            Detach::Hold { grace, to } => {
                // Park it: keep the row (under its STABLE player key —
                // nothing is re-keyed), the entity, the world state, the
                // group membership AND the cap slot (§4 — members
                // accounting does not drop). The binding row stays too: the
                // parked row still belongs to that session until a resume
                // re-points it. The clock is CORE-owned (§14.4): the grace
                // and the veto ceiling are written here as absolute
                // instants and the phase-0c sweep reads them.
                let ceiling = self.config.max_detach_hold;
                let rc = self.conns.get_mut(&player).expect("guarded by caller");
                rc.park(grace, to, ceiling, crate::ticker::now());
                // OFF the input-idle clock while parked: the row has no
                // live input source, so counting its silence would
                // double-count a member the detach machinery already owns
                // (and would let the idle ceiling fire on top of a hold).
                // A resume restarts the clock.
                self.idle.stop(player);
                // Today's leave semantics for in-flight work (§11 "RPC
                // pending detach anında"): pending requests drop, late
                // reports are silently discarded (structural already),
                // queued answers for the dead session go.
                self.drop_conn_request_state(conn);
                debug!(
                    room = %self.config.id,
                    %conn,
                    %player,
                    ?grace,
                    ?to,
                    "player detached (entity parked)"
                );
            }
        }
    }

    pub(in crate::room) fn handle_control(&mut self, c: RoomControl) -> bool {
        match c {
            RoomControl::Join { conn, out, reply } => {
                // A plain (unidentified) join: the registry routes every
                // NON-empty identity through `Resume` instead, so this
                // arm is the anonymous one by construction.
                self.admit_fresh(conn, String::new(), out, reply)
            }
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
                    // A row that is ALREADY parked has had its policy run
                    // once; a second Detach for it is a duplicate (the
                    // transport of an idle-expired member dying later is
                    // exactly that shape) and must not re-ask the policy.
                    && !self.conns.get(&player).is_some_and(|c| c.detached)
                {
                    self.detach_player(player, conn, &identity, true);
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
                    return self.admit_fresh(conn, identity, out, reply);
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
                            return self.admit_fresh(conn, identity, out, reply);
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
                            return self.admit_fresh(conn, identity, out, reply);
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
                        self.admit_fresh(conn, identity, out, reply)
                    }
                    ResumeFound::Never => self.admit_fresh(conn, identity, out, reply),
                }
            }
            RoomControl::Shutdown => false,
        }
    }
}
