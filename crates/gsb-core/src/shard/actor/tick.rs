//! The shard's tick body: the room's five phases plus MIGRATE, BORDER
//! and TEAMS. Each phase that carries weight is a child module.

use std::fmt::Debug;
use std::hash::Hash;

use prost::Message;
use tracing::{debug, warn};

use crate::id::PlayerId;
use crate::room::{Action, IdleView, TickCtx};
use crate::rpc::{RPC_REQ_OP, RpcRequest};
use crate::ticker::TickInfo;

use crate::shard::actor::ShardActor;
use crate::shard::*;

mod border;
mod completions;
mod detach;
mod effects;
mod idle;
mod migrate;
mod requests;
mod teams;

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
    /// The six phases. Synchronous. Returns `false` when the actor should
    /// stop.
    pub(crate) fn step_phases(&mut self, t: &TickInfo) -> bool {
        // -- time (same catch-up discipline as the room actor).
        let dt = {
            let last = self.last_at.replace(t.at);
            match last {
                Some(last) => {
                    let elapsed = t.at.saturating_duration_since(last);
                    let cap = self.config.period() * self.config.max_catchup;
                    if elapsed > cap {
                        warn!(
                            room = %self.config.id,
                            shard = self.index,
                            ?elapsed,
                            ?cap,
                            "long stall: catch-up dt clamped (sim temporarily \
                             slower than real time)"
                        );
                        cap
                    } else {
                        elapsed
                    }
                }
                None => self.config.period(),
            }
        };

        // -- Tombstone TTL sweep (lazy, CONTROL). Bounded cost: the
        //    tombstone table only ever holds leaves of the last TTL
        //    window (the epoch table is pruned on leave), so this O(n)
        //    `retain` runs over a small set and amortizes to noise.
        //
        //    Why expiry preserves the race gate (the load-bearing
        //    argument): a Migrate processed AFTER its tombstone expired
        //    must have been enqueued more than TOMBSTONE_TTL_TICKS ticks
        //    after the corresponding leave — Migrates are generated only
        //    while the entity is alive (leave-processing despawns it) and
        //    travel into a BOUNDED per-shard FIFO inbox drained by every
        //    CONTROL phase, so queue residence beyond TTL ticks means
        //    this shard itself is stalled far beyond any healthy
        //    operating point. The pre-existing degradation notes (the
        //    one-tick alignment, the bounded blink — module docs) already
        //    assume non-stalled shards; inside that envelope no genuinely
        //    racing Migrate is still in flight when its tombstone expires.
        if self
            .last_tombstone_sweep
            .is_none_or(|at| t.tick.saturating_sub(at) >= TOMBSTONE_SWEEP_EVERY_TICKS)
        {
            let now = t.tick;
            self.conn_tombstone
                .retain(|_, (_, wrote)| now.saturating_sub(*wrote) < TOMBSTONE_TTL_TICKS);
            self.last_tombstone_sweep = Some(now);
        }

        // -- Phase 0 — CONTROL (drain the shard channel; the same messages
        //    the room handles on its control channel, plus the shard
        //    protocol — Migrate / Border). Deferred migrations (their
        //    install gate has not opened yet — see `handle_msg`) are
        //    re-offered FIRST, in send order.
        let deferred = std::mem::take(&mut self.deferred);
        for m in deferred {
            if !self.handle_msg(m, t.tick) {
                return false;
            }
        }
        // The drain itself crosses the ShardLink seam (`docs/DISTRIBUTED.md`
        // §3). Pulling everything queued NOW into a vec and then handling is
        // observably identical to the old interleaved `try_recv` loop:
        // nothing in `handle_msg` enqueues into THIS shard's own inbox
        // synchronously (sends go to neighbors' inboxes), and on Shutdown
        // any leftovers die with the actor's inbox either way.
        for m in self.inbox.drain() {
            if !self.handle_msg(m, t.tick) {
                return false;
            }
        }

        // -- Phase 0d — EFFECTS IN: the remote effects the drain collected
        //    apply now — after every Migrate of the drain installed its
        //    entity, before this tick's input and the detach sweep (so a
        //    cross-seam hit is in the world the combat veto reads).
        self.phase_effects_in(t.tick);

        self.phase_completions();

        self.phase_detach_sweep();

        self.phase_idle_sweep(t.at);
        // -- Phase 1 — READ (the room's bounded pull: per-connection
        //    fairness budget + shard-level pull budget).
        let per_conn = self.config.max_actions_per_conn_per_tick;
        let mut budget = self.config.max_pending_actions;
        let mut actions: Vec<Action> = Vec::new();
        // Players that delivered input this tick — stamped into the
        // input-idle clock after the pull (the clock is a sibling field
        // of `conns`, which the loop holds mutably). The buffer is a
        // local: it only ever holds THIS tick's active members, and a
        // quiet shard never pushes to it.
        let mut acted: Vec<PlayerId> = Vec::new();
        for (&player, r) in self.conns.iter_mut() {
            // A detached (or bot-fed) row has no live input source; skip
            // it exactly like the room's rotation does.
            if r.detached {
                continue;
            }
            let mut pulled = 0usize;
            for _ in 0..per_conn {
                if budget == 0 {
                    break;
                }
                match r.actions.try_recv() {
                    Ok(a) => {
                        budget -= 1;
                        pulled += 1;
                        actions.push(a);
                    }
                    Err(_) => break,
                }
            }
            if pulled > 0 {
                acted.push(player);
            }
            if budget == 0 {
                break;
            }
        }
        // The INPUT-IDLE stamp (the room actor's READ-phase stamp,
        // mirrored): the pull above IS the structural definition of
        // "action-bearing" — a heartbeat is answered in the connection
        // actor and never reaches this channel. One stamp per member that
        // actually delivered input; a silent member costs nothing.
        for player in acted {
            self.idle.touch(player, t.at);
        }

        // -- Phase 1.5 — BINDING TRANSLATION: byte-for-byte the room
        //    actor's step (Faz 2 — ONE mechanism on both actors; see the
        //    room's phase comment for the full rationale): every pulled
        //    action still names its transport session, and THIS table is
        //    the one authority for the conn ↔ PlayerId context. An
        //    unbound conn drops here — after a resume the old session has
        //    no binding row left, so its stray frames can never reach the
        //    world.
        actions.retain_mut(|a| match self.binding.get(&a.conn) {
            Some(&player) => {
                a.player = player;
                true
            }
            None => {
                debug!(
                    room = %self.config.id,
                    shard = self.index,
                    conn = %a.conn,
                    op = a.op,
                    "action dropped: connection not bound (stale/old session)"
                );
                false
            }
        });

        // -- Phase 2a — SPLIT the requests out of the pulled actions (the
        //    Faz 3 RPC promotion; byte-for-byte the room actor's split).
        //    A request is an action carrying the base-band envelope
        //    opcode; this actor decodes the envelope (a base message) and
        //    hands the rest to the logic at 2c. The split is in-order:
        //    both lists keep arrival order, and ALL fire-and-forget
        //    actions of the tick ingest BEFORE any request is handed to
        //    `handle_request` (the ordering contract of `crate::rpc`: a
        //    request sees the world after this tick's actions were
        //    applied). Cost when quiet: one u16 compare per pulled action.
        //
        //    A malformed envelope (or id = 0, which cannot correlate) is a
        //    NORMAL rejection: answered this tick, counted in the
        //    malformed bucket — a client bug, not a protocol violation.
        let mut requests: Vec<RpcRequest> = Vec::new();
        actions.retain_mut(|a| {
            if a.op != RPC_REQ_OP {
                return true;
            }
            match gsb_protocol::base::RpcRequest::decode(&a.payload[..]) {
                Ok(env) if env.id != 0 => {
                    requests.push(RpcRequest {
                        conn: a.conn,
                        // Already translated (phase 1.5): the logic sees
                        // the stable player key, while pending/replies
                        // stay session-scoped under `conn`.
                        player: a.player,
                        id: env.id,
                        // The wire type is `u32`; the op space is `u16`
                        // by protocol contract (same cast as the room).
                        op: env.op as u16,
                        payload: env.payload.into(),
                    });
                    false
                }
                // A congested connection at its cap: refused, like at 2c
                // (the room's rule, F14).
                _ if self.refuses_congested(a.conn, a.player) => {
                    self.m.requests_rejected_conn_cap += 1;
                    false
                }
                _ => {
                    self.m.requests_rejected_malformed += 1;
                    self.queue_reply(
                        a.conn,
                        0,
                        a.op,
                        false,
                        "malformed request envelope (or correlation id = 0)".to_string(),
                        bytes::Bytes::new(),
                    );
                    false
                }
            }
        });

        // -- The tick context is built HERE, after every phase that
        //    writes the idle clock has run, because it LENDS that clock to
        //    the logic (`ctx.since_input`). The clock is moved out of the
        //    actor for the body: the phases below take `&mut self`, which
        //    a borrow living inside `ctx` would forbid. O(1) pointer
        //    swap; nothing below reads or writes the clock, and no phase
        //    below returns early, so the restore is unconditional.
        let idle = std::mem::take(&mut self.idle);
        let ctx = TickCtx {
            room: self.config.id,
            tick: t.tick,
            dt,
            idle: IdleView::new(&idle, t.at),
        };

        // -- Phase 2b — CONVERT, with the cross-seam view (the borrowed
        //    strip read in place, the effect outbox — CROSS-SHARD §2).
        self.logic.ingest_seam(
            &mut self.world,
            &ctx,
            &mut actions,
            &mut self.effects.seam(&self.border, &self.lenders),
        );

        self.phase_requests(&requests, &ctx);
        // -- Phase 3 — SYSTEMS (same view).
        self.logic.update_seam(
            &mut self.world,
            &ctx,
            &mut self.effects.seam(&self.border, &self.lenders),
        );
        // -- Phase 3b — EFFECTS OUT: what the hooks emitted (and what
        //    phase 0d forwarded) leaves for its authority.
        self.phase_effects_out(t.tick);

        self.phase_migrate(t);

        self.phase_border(t);
        // The borrowed boundary set, flattened once for the two phases
        // that read it.
        let borrowed = self.borrowed_view();
        // -- Phase 5b — TEAMS (`docs/CROSS-SHARD.md` §8b): the logic reads
        //    the other shards' team records and hands back this shard's
        //    export for the registry hub.
        self.phase_teams(&ctx, &borrowed);
        // -- Phase 6 — BROADCAST (the room's broadcast phase with the
        //    borrowed boundary set folded into every group's snapshot).
        self.broadcast_phase(&ctx, &borrowed);
        // The lend is over (NLL ends `ctx`'s borrow at its last use);
        // hand the clock back to the actor.
        self.idle = idle;
        true
    }

    /// Handle one shard-channel message (phase 0). Returns `false` when
    /// the actor should stop.
    /// Faz C test lever: force per-neighbor exchange modes (index-aligned
    /// with `links`). Production never calls this — modes derive from the
    /// link class (`InProcLink` ⇒ AlwaysFull).
    #[cfg(test)]
    pub(crate) fn force_exchange_modes(&mut self, modes: Vec<ExchangeMode>) {
        self.exchange_override = Some(modes);
    }
}
