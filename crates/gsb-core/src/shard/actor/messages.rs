//! The shard mailbox's message handling: join, detach, resume, leave,
//! migrate-in, the border exchange and the team imports.
//!
//! NOT split further: this is one match over `ShardMsg`, and its arms
//! share the epoch/binding guards that make the ordering argument
//! readable. Splitting it scatters that argument across files for a
//! line count.

use std::collections::hash_map::Entry;
use std::fmt::Debug;
use std::hash::Hash;
use std::time::Instant;

use tracing::{debug, warn};

use crate::error::CoreError;
use crate::room::{ExpireTo, ResumeFound, RoomConn};

use crate::shard::actor::ShardActor;
use crate::shard::*;

impl<W, G, St, Sp> ShardActor<W, G, St, Sp>
where
    W: Send + 'static,
    G: Eq + Hash + Clone + Debug + Send + 'static,
    St: Debug + Send + 'static,
    // The strip rides every exchange and view; the bounds mirror what
    // the delta protocol does with it (diff via PartialEq, clone into
    // each neighbor's message, store in the actor's maps).
    Sp: Debug + Clone + PartialEq + Send + 'static,
{
    pub(crate) fn handle_msg(&mut self, m: ShardMsg<St, Sp>, tick: u64) -> bool {
        match m {
            ShardMsg::Join {
                conn,
                epoch,
                identity,
                out,
                reply,
            } => {
                // A join supersedes any stale state this connection had
                // (same as the room actor) — including its request state
                // (a rejoin is a NEW session: in-flight requests and
                // queued answers of the old one are dropped; their late
                // worker reports are discarded by the 0b reconciliation).
                // A queued leave request of this connection would settle
                // the membership it now holds again (B40): it goes.
                self.leave_requests.retain(|r| r.conn != conn);
                if let Some(&stale) = self.binding.get(&conn) {
                    let _ = self.conns.remove(&stale); // old halves drop
                    self.drop_conn_request_state(conn);
                    self.logic.on_leave(&mut self.world, stale);
                }
                // Identity-space exhaustion guard (structurally unreachable
                // at the default capacity — see module docs): the shard
                // refuses rather than draw past its bound.
                if self.logic.serial_used() + 1 >= self.logic.serial_capacity() {
                    warn!(
                        room = %self.config.id,
                        shard = self.index,
                        %conn,
                        "shard wire-id serials exhausted; join rejected as \
                         RoomFull (raise the bound: SHARD_SERIAL_CAPACITY)"
                    );
                    let _ = reply.send(Err(CoreError::RoomFull(self.config.id.0)));
                    return true;
                }
                // The LOGIC mints the stable player identity (Faz 2).
                let admission = self.logic.on_join_as(&mut self.world, conn, &identity);
                self.m.joins += 1;
                let (act_tx, act_rx) = self.config.action_channel();
                self.conn_epoch.insert(conn, epoch);
                self.binding.insert(conn, admission.player);
                self.conns.insert(
                    admission.player,
                    RoomConn {
                        conn,
                        identity,
                        out,
                        actions: act_rx,
                        entity: admission.entity,
                        group: self.logic.group_of(&self.world, admission.player),
                        batch: Vec::new(),
                        dropping: false,
                        detached: false,
                        detach_deadline: None,
                        detach_ceiling: None,
                        expire_to: ExpireTo::Despawn,
                        bot_fed: false,
                        session_epoch: 0,
                    },
                );
                // The input-idle clock starts at the join (the room
                // actor's rule, mirrored).
                self.idle.start(admission.player, crate::ticker::now());
                let _ = reply.send(Ok((admission.entity, act_tx)));
                debug!(
                    room = %self.config.id,
                    shard = self.index,
                    %conn,
                    player = %admission.player,
                    entity = admission.entity,
                    epoch,
                    "player joined shard"
                );
                true
            }
            ShardMsg::Detach {
                conn,
                entity,
                identity,
            } => {
                // The registry's close broadcast: exactly the owning shard
                // runs the policy; the binding + entity guards make the
                // others no-ops (the same shape as a broadcast `Leave`).
                if let Some(&player) = self.binding.get(&conn)
                    && self.conns.get(&player).map(|c| c.entity) == Some(entity)
                    // A row that is ALREADY parked has had its policy run
                    // once; a second Detach for it is a duplicate (the
                    // room actor's guard, mirrored).
                    && !self.conns.get(&player).is_some_and(|c| c.detached)
                {
                    self.detach_player(player, conn, &identity, true);
                }
                true
            }
            ShardMsg::Resume {
                conn,
                epoch,
                identity,
                out,
                reply,
            } => {
                // §6 broadcast-resume: this shard accepts ONLY if its
                // ledger holds the identity — every other shard answers
                // "not here" without touching anything.
                let outcome = match self.logic.resume_lookup(&self.world, &identity) {
                    ResumeFound::Held(player) => {
                        // The parked row, by its STABLE key (Faz 2): one
                        // lookup instead of the pre-Faz-2 scan.
                        match self.conns.get(&player) {
                            Some(rc) if rc.detached => {
                                // Copy the guard/reply values out so the
                                // table borrow ends before the rebind.
                                let rc_epoch = rc.session_epoch;
                                let entity = rc.entity;
                                // Epoch guard (§7), one comparison — see
                                // the room actor's Resume arm for the full
                                // rationale.
                                if epoch != 0 && rc_epoch != 0 && epoch <= rc_epoch {
                                    self.m.resume_rejected_stale += 1;
                                    warn!(
                                        room = %self.config.id,
                                        shard = self.index,
                                        %player,
                                        %identity,
                                        resume_epoch = epoch,
                                        "resume rejected: stale epoch"
                                    );
                                    Err(CoreError::ResumeStale)
                                } else {
                                    let act_tx =
                                        self.rebind_session(player, conn, epoch, &identity, out);
                                    Ok(Some((entity, act_tx)))
                                }
                            }
                            _ => {
                                // Ledger/table divergence (the hold expired
                                // in this very tick's sweep, or the row is
                                // already live): counted stale; the
                                // dispatcher's all-miss fallback turns it
                                // into a transparent fresh join.
                                self.m.resume_rejected_stale += 1;
                                Ok(None)
                            }
                        }
                    }
                    ResumeFound::Ended => {
                        // Mechanism-level rejection; the dispatcher turns an
                        // all-shards-miss outcome into the transparent fresh
                        // join (§5). Counted so operators see how many
                        // attempts raced (or followed) a hold's end.
                        self.m.resume_rejected_stale += 1;
                        Ok(None)
                    }
                    ResumeFound::Never => Ok(None),
                };
                let _ = reply.send(outcome);
                true
            }
            ShardMsg::Leave {
                conn,
                entity,
                epoch,
            } => {
                // Stale-leave guard (binding + entity id): only the entity
                // this player currently owns.
                if let Some(&player) = self.binding.get(&conn)
                    && self.conns.get(&player).map(|c| c.entity) == Some(entity)
                {
                    self.despawn_conn(player, false);
                    self.m.leaves += 1;
                    debug!(
                        room = %self.config.id,
                        shard = self.index,
                        %conn,
                        %player,
                        entity,
                        "player left shard"
                    );
                }
                // Prune the epoch entry — in BOTH arms (the entity-matched
                // despawn above and the broadcast leave this shard had no
                // entity for). Safety: `conn_epoch` is read at exactly one
                // place — stamping an outgoing Migrate for a connection
                // LIVE in `self.conns` — and once a leave of the live
                // join's epoch is processed here, that connection cannot
                // be live on this shard any more (either the arm above
                // just despawned it, or it never lived here). Any future
                // migrate-in re-inserts the entry (see the Migrate arm),
                // and a re-JOIN is safe because the per-connection
                // dispatcher serializes ops: the rejoin carries a strictly
                // newer epoch and is processed (on this FIFO shard
                // channel) before any newer migrate-out could stamp from
                // here. Without this removal the table grew by every
                // connection that ever joined.
                self.conn_epoch.remove(&conn);
                // Leave tombstone (see module docs): a late `Migrate` of
                // the join this leave ends must be rejected here — and in
                // every other shard that also saw the leave (it is
                // broadcast to all of them). The tombstone table is kept
                // SEPARATE from `conn_epoch` (the installed join's
                // epoch): a migration of an *alive* join carries that
                // epoch legitimately and must not be mistaken for dead.
                match self.conn_tombstone.entry(conn) {
                    Entry::Occupied(mut e) => {
                        if e.get().0 < epoch {
                            // Max-update BOTH halves together: the write
                            // tick belongs to the leave that owns the
                            // surviving (highest) epoch — see the field
                            // docs. An equal-or-stale leave keeps the
                            // older entry whole (conservative: its guard,
                            // being for an equal-or-newer death, lives
                            // longer).
                            e.insert((epoch, tick));
                        }
                    }
                    Entry::Vacant(e) => {
                        e.insert((epoch, tick));
                    }
                }
                true
            }
            ShardMsg::Migrate {
                from,
                at_tick,
                wire,
                state,
                player,
            } => {
                // Install gate (module docs, "Migration protocol"): the
                // crossing was sampled by the sender at tick `at_tick` and
                // takes effect here at `at_tick + 1` — the explicit
                // one-tick alignment. A message that arrived EARLY
                // (in-process delivery: the sender's tick body and this
                // shard's tick body interleave on the runtime) is
                // deferred until the gate opens — installing it now would
                // put the entity in two shards for one tick.
                if tick <= at_tick {
                    self.deferred.push_back(ShardMsg::Migrate {
                        from,
                        at_tick,
                        wire,
                        state,
                        player,
                    });
                    return true;
                }
                // The epoch gate: reject a `Migrate` whose join this shard
                // already knows to be dead (a leave of that epoch — or a
                // newer one — was processed first; the leave/migration
                // race, in EITHER order). The gate reads the TOMBSTONE
                // table, not `conn_epoch`: a migration of an alive join
                // carries the installed epoch legitimately. Only the
                // epoch half of the tuple gates; the tick half is the
                // sweep's bookkeeping.
                if let Some(p) = &player
                    && let Some((tomb_epoch, _wrote_at)) = self.conn_tombstone.get(&p.conn)
                    && *tomb_epoch >= p.epoch
                {
                    debug!(
                        room = %self.config.id,
                        shard = self.index,
                        %from,
                        wire,
                        conn = %p.conn,
                        "migrate dropped: the join is dead (leave \
                         processed first)"
                    );
                    return true;
                }
                self.logic.on_migrate_in(
                    &mut self.world,
                    wire,
                    state,
                    player.as_ref().map(|p| p.player),
                );
                self.m.migrations_in += 1;
                // Back home: this shard is the entity's authority again,
                // so an effect for it must no longer be handed on.
                self.effects.forwarded.remove(&wire);
                if let Some(p) = player {
                    // The player moves here: the out channel and the
                    // action inbox were MOVED with the message (ownership
                    // transfer — the connection actor never notices), and
                    // the binding row is installed so control broadcasts
                    // find this shard. Invariant (see module docs): after
                    // passing the epoch gate this shard cannot already
                    // hold the player.
                    debug_assert!(!self.conns.contains_key(&p.player));
                    self.conn_epoch.insert(p.conn, p.epoch);
                    self.binding.insert(p.conn, p.player);
                    // The input-idle stamp rode along: restore it rather
                    // than restarting the clock, or a shard crossing would
                    // silently forgive an idle player.
                    if let Some(at) = p.last_input {
                        self.idle.start(p.player, at);
                    }
                    self.conns.insert(
                        p.player,
                        RoomConn {
                            conn: p.conn,
                            identity: p.identity,
                            out: p.out,
                            actions: p.actions,
                            entity: p.entity,
                            group: self.logic.group_of(&self.world, p.player),
                            batch: Vec::new(),
                            dropping: false,
                            detached: p.detached,
                            detach_deadline: p.detach_deadline,
                            detach_ceiling: p.detach_ceiling,
                            expire_to: p.expire_to,
                            bot_fed: p.bot_fed,
                            session_epoch: p.session_epoch,
                        },
                    );
                }
                debug!(
                    room = %self.config.id,
                    shard = self.index,
                    %from,
                    wire,
                    at_tick,
                    "entity migrated in"
                );
                true
            }
            ShardMsg::Border { from, exchange } => {
                // The receiver half of the §6.4 protocol: a Full replaces
                // the view and re-baselines the expected sequence (accepted
                // at ANY time — the rebuilt-incarnation path); a Delta is
                // applied atomically ONLY on an exact sequence match, and a
                // mismatch rejects it wholesale, requests a resync and
                // quarantines the view until the healing Full arrives.
                //
                // measurement scaffolding for CROSS-SHARD §7 — remove or
                // promote after the delta decision: apply-side accounting.
                // In process there is no decode step (the message arrives
                // as structs); the map insert/remove IS the "decode +
                // apply" cost a wire deployment would pay on top of its
                // deserialization. Accounted through the same helpers as
                // the sender for comparability.
                // First exchange from this neighbour: it joins the
                // cross-seam view's lookup order (ascending, so the
                // answer to "who lends this wire" is deterministic).
                if let Err(at) = self.lenders.binary_search(&from) {
                    self.lenders.insert(at, from);
                }
                let tr = Instant::now();
                let s = &mut self.bstats;
                match exchange {
                    BorderExchange::Full { seq, entities, .. } => {
                        let recs = entities.len();
                        let bytes = border_payload_len(entities.iter());
                        let view = self.border.entry(from).or_default();
                        view.recs = entities.into_iter().map(|r| (r.wire, r)).collect();
                        view.expected_seq = seq.wrapping_add(1);
                        view.stale_until_full = false;
                        s.imports += 1;
                        s.import_records += recs as u64;
                        s.import_bytes += bytes;
                    }
                    BorderExchange::Delta {
                        seq,
                        upserts,
                        exits,
                        ..
                    } => {
                        let view = self.border.entry(from).or_default();
                        if view.stale_until_full || seq != view.expected_seq {
                            // Continuity broken (a lost exchange, or this is
                            // a stale incarnation's message after a rebuild):
                            // never apply — an unknown-sized hole can leave
                            // ghosts and stale positions that no later delta
                            // can name. Quarantine + ask upstream for a Full
                            // (pin 3a). The request itself rides the same
                            // bounded mailbox with try_send: if THAT drops,
                            // the periodic cadence (3c) still heals us, just
                            // slower.
                            view.stale_until_full = true;
                            s.resync_requests_sent += 1;
                            // Only a FULL link logs: the transient case is
                            // worth a line, a closed one means the peer
                            // incarnation is gone and the death watcher
                            // already owns that story.
                            if let Some(link) = self.links.get_mut(from)
                                && let Err(e) =
                                    link.send(ShardMsg::ResyncRequest { from: self.index })
                                && matches!(e, LinkFull::Full { .. })
                            {
                                debug!(
                                    room = %self.config.id,
                                    shard = self.index,
                                    %from,
                                    "resync request dropped (neighbor channel \
                                     full); the periodic Full remains the \
                                     backstop"
                                );
                            }
                        } else {
                            let ups = upserts.len();
                            let exs = exits.len();
                            let bytes = delta_payload_len(&upserts, exs);
                            for r in upserts {
                                view.recs.insert(r.wire, r);
                            }
                            for w in exits {
                                view.recs.remove(&w);
                            }
                            view.expected_seq = seq.wrapping_add(1);
                            view.stale_until_full = false;
                            s.imports += 1;
                            s.import_records += (ups + exs) as u64;
                            s.import_bytes += bytes;
                        }
                    }
                }
                s.import_us += tr.elapsed().as_micros() as u64;
                true
            }
            ShardMsg::ResyncRequest { from } => {
                // A neighbor rejected our delta stream: serve it a Full on
                // the next phase 5 (the flag makes the answer deterministic
                // — no timing race, and the Full is generated after every
                // already-queued message, so ordering stays natural).
                let st = self.export.entry(from).or_default();
                st.needs_full = true;
                st.resync_requested = true;
                true
            }
            ShardMsg::RemoteEffect(effect) => {
                // Collected, not applied: the whole CONTROL drain lands
                // first (a Migrate installing the target may sit behind
                // it in the FIFO), then `phase_effects_in` applies the
                // due ones in their deterministic order.
                self.effects.stats.received += 1;
                self.effects.pending.push(effect);
                true
            }
            ShardMsg::TeamImport(import) => {
                // Applied in place (a slot replacement — no install gate:
                // the records are visibility, not ownership). The logic
                // reads the merged view in the TEAMS phase.
                let records = import.records.len() as u64;
                let cut = self.teams.insert(import) as u64;
                self.tstats.imports += 1;
                self.tstats.import_records += records - cut;
                self.tstats.over_cap += cut;
                true
            }
            ShardMsg::Shutdown => false,
        }
    }
}
