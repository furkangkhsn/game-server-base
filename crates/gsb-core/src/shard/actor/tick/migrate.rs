//! Phase 4 — MIGRATE: entities crossing this shard's edge.

use std::fmt::Debug;
use std::hash::Hash;

use tracing::warn;

use crate::room::RoomConn;
use crate::ticker::TickInfo;

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
    /// Tick phase 4 — MIGRATE: entities that crossed this shard's edge
    /// leave for their new owner, one hop, one owner at a time.
    pub(crate) fn phase_migrate(&mut self, t: &TickInfo) {
        // -- Phase 4 — MIGRATE.
        //    4a. Despawn the entities marked out by a SUCCESSFUL send last
        //        tick (the mark's tick index is this one — the protocol's
        //        exactly-once boundary, see module docs).
        if !self.pending_out.is_empty() {
            let mut due = Vec::new();
            let mut rest = Vec::new();
            for (wire, despawn_at) in self.pending_out.drain(..) {
                if despawn_at <= t.tick {
                    due.push(wire);
                } else {
                    rest.push((wire, despawn_at));
                }
            }
            for wire in due {
                self.logic.on_migrate_out(&mut self.world, wire);
            }
            self.pending_out = rest;
        }
        //    4b. Collect this tick's crossings and send them. A failed
        //        send (neighbor's channel full — the neighbor stalled for
        //        ~seconds) is NOT marked: the entity stays here and is
        //        re-collected on the next tick (the crossing is a function
        //        of position, which is unchanged until it moves back). The
        //        failed send hands the message back (`TrySendError`
        //        carries it), so the moved connection halves are rolled
        //        back into the connection table — a failed migration
        //        orphans nothing.
        let nb = self.logic.neighbors().to_vec();
        for b in nb {
            let migrations = self.logic.collect_migrations(&mut self.world, b);
            for mig in migrations {
                let player = mig.player.and_then(|p| {
                    let entry = self.conns.remove(&p)?;
                    // The input-idle stamp travels with the row and this
                    // shard's clock loses it (a rolled-back send below
                    // puts it straight back).
                    let last_input = self.idle.last(p);
                    self.idle.stop(p);
                    // The binding row travels too (the receiving shard
                    // installs its own): the session stays bound to this
                    // player across the move, so control broadcasts still
                    // find the owner.
                    self.binding.remove(&entry.conn);
                    Some(Box::new(PlayerMigration {
                        player: p,
                        conn: entry.conn,
                        // The epoch of the join this entity belongs to: the
                        // shard that last installed it recorded it.
                        epoch: self.conn_epoch.get(&entry.conn).copied().unwrap_or(0),
                        out: entry.out,
                        actions: entry.actions,
                        entity: entry.entity,
                        // A PARKED player's entity migrates like any other;
                        // its detach flags ride along so the receiving shard
                        // keeps skipping its dead halves (§3.2 + §7).
                        detached: entry.detached,
                        detach_deadline: entry.detach_deadline,
                        expire_to: entry.expire_to,
                        bot_fed: entry.bot_fed,
                        session_epoch: entry.session_epoch,
                        identity: entry.identity,
                        last_input,
                    }))
                });
                // The session whose request state dies with a COMMITTED
                // move (the Ok arm below); read before the send consumes
                // the message.
                let moving_conn = player.as_ref().map(|pm| pm.conn);
                match self.links[b].send(ShardMsg::Migrate {
                    from: self.index,
                    at_tick: t.tick,
                    wire: mig.wire,
                    state: mig.state,
                    player,
                }) {
                    Ok(()) => {
                        // The move committed: the session's RPC state does
                        // NOT travel (module docs, "Shard-RPC and
                        // match-result") — the worker futures of its
                        // in-flight requests were spawned HERE and report to
                        // THIS shard's completion channel, so carrying
                        // pending entries across would need cross-actor
                        // report forwarding. Same §11 posture as detach:
                        // session-scoped request state dies with the
                        // session's ownership move; stale reports land late
                        // here and are counted `requests_late`; the player
                        // re-requests on the receiving shard under a fresh
                        // id. (Dropped AFTER a successful send — on a failed
                        // one below, the connection is rolled back whole and
                        // keeps its in-flight work.)
                        if let Some(mc) = moving_conn {
                            self.drop_conn_request_state(mc);
                        }
                        self.pending_out.push((mig.wire, t.tick + 1));
                    }
                    // The link refused and handed the message back — the
                    // same value the raw TrySendError used to carry.
                    Err(full) => {
                        let msg = full.into_msg();
                        // Roll back the player's move (the entity stays;
                        // the row must remain registered and pull
                        // its input here until the retry lands) — including
                        // the binding row the send removed.
                        if let ShardMsg::Migrate {
                            player: Some(p),
                            wire,
                            ..
                        } = msg
                        {
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
                                    detached: p.detached,
                                    detach_deadline: p.detach_deadline,
                                    expire_to: p.expire_to,
                                    bot_fed: p.bot_fed,
                                    session_epoch: p.session_epoch,
                                },
                            );
                            self.binding.insert(p.conn, p.player);
                            if let Some(at) = p.last_input {
                                self.idle.start(p.player, at);
                            }
                            warn!(
                                room = %self.config.id,
                                shard = self.index,
                                neighbor = b,
                                %wire,
                                "migrate send failed (neighbor channel \
                                 full); the entity stays, the connection \
                                 is rolled back, and the crossing is \
                                 retried next tick"
                            );
                        }
                    }
                }
            }
        }
    }
}
