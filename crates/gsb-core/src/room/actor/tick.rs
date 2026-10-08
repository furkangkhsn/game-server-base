//! The five-phase tick body. Each phase that carries real weight is a
//! child module; this file is the order they run in.

use crate::room::parts;
use crate::room::*;
use crate::ticker::TickInfo;
use std::fmt::Debug;
use std::hash::Hash;
use tracing::warn;

use crate::room::actor::RoomActor;

mod completions;
mod detach;
mod idle;
mod kick;
mod read;

impl<W, G, Sp> RoomActor<W, G, Sp>
where
    G: Eq + Hash + Clone + Debug,
    // The strip payload's trait bounds (`GameLogic::Strip`) — restated so
    // calls through the logic object type-check; the room itself never
    // touches the payloads.
    Sp: Debug + Clone + PartialEq + Send + 'static,
{
    /// The five phases (moved out of [`Self::step`] so the whole body is
    /// measurable). Synchronous. Returns `false` when the actor should
    /// stop.
    pub(in crate::room) fn step_phases(&mut self, t: &TickInfo) -> bool {
        // -- time: wall-clock since the last step; covers missed ticks
        //    (frame-rate independent), capped for pathological stalls.
        let dt = {
            let last = self.last_at.replace(t.at);
            match last {
                Some(last) => {
                    let elapsed = t.at.saturating_duration_since(last);
                    let cap = self.config.period() * self.config.max_catchup;
                    if elapsed > cap {
                        warn!(
                            room = %self.config.id,
                            ?elapsed,
                            ?cap,
                            "long stall: catch-up dt clamped (sim temporarily slower than real time)"
                        );
                        cap
                    } else {
                        elapsed
                    }
                }
                None => self.config.period(),
            }
        };
        // The room clock (`TickCtx::time`): the sum of the steps' dt.
        self.clock += dt;

        // -- Phase 0 — CONTROL (before actions: a fresh join's action
        //    channel is only registered once its Join has been processed).
        while let Ok(c) = self.control_rx.try_recv() {
            if !self.handle_control(c) {
                return false;
            }
        }

        self.phase_completions();

        self.phase_detach_sweep();

        self.phase_idle_sweep(t.at);

        // -- Phase 0e — EVENTS (`docs/EVENTS.md` §3): the room's OWN event
        //    mailbox into the table, after every CONTROL-side change of
        //    membership (joins, leaves, the hold and idle sweeps) and
        //    before READ — so a system that takes events before requests
        //    checks this tick's input against them. Quiet: one empty
        //    `try_recv`.
        {
            let logic = &self.logic;
            let world = &self.world;
            self.events
                .drain(&self.conns, &self.calls, &|p| logic.call_owner(world, p));
        }

        let mut actions = self.phase_read(t.at);

        // -- The tick context is built HERE, after every phase that
        //    writes the idle clock has run, because it LENDS that clock to
        //    the logic (`ctx.since_input`). The clock is moved out of the
        //    actor for the duration of the body: the phases below take
        //    `&mut self`, which a borrow living inside `ctx` would
        //    forbid. The move is an O(1) pointer swap, nothing between
        //    here and the restore reads or writes the clock, and no phase
        //    below this point returns early — so the restore is
        //    unconditional. The context lends the tick's kick queue
        //    too (E8): a local, empty and allocation-free unless a hook
        //    kicks.
        let kicks = KickQueue::default();
        // The generator is lent like the clock (the phases below take
        // `&mut self`); it comes back with it at the end of the step.
        let rng = std::mem::take(&mut self.rng);
        let idle = std::mem::take(&mut self.idle);
        let paths = std::mem::take(&mut self.paths);
        let ctx = TickCtx {
            room: self.config.id,
            tick: t.tick,
            dt,
            time: self.clock,
            rng: rng.view(),
            idle: crate::room::IdleView::new(&idle, t.at),
            kicks: kicks.kicks(),
            paths: paths.view(self.config.period()),
        };
        // -- Phase 2a — split the requests out of the pulled actions (the
        //    RPC pattern, see `crate::rpc`), then READ: a request whose op
        //    the call table claims goes into the table now (typed, bound
        //    to its owner — `docs/SERVICES.md` §5); the rest wait for 2c.
        //    Both keep their arrival order (`crate::room::Parts::split`).
        let mut requests = parts!(self).split(&mut actions);
        {
            let logic = &self.logic;
            let world = &self.world;
            parts!(self).read(&mut requests, &|p| logic.call_owner(world, p));
        }

        // -- Phase 2b — CONVERT: actions → component writes (game logic).
        //    All fire-and-forget actions are processed before the
        //    requests (the ordering contract; a request sees the world
        //    after this tick's actions were applied).
        #[cfg(feature = "record")]
        self.calls.note(|| {
            crate::determinism::Input::Actions(
                actions
                    .iter()
                    .map(|a| crate::determinism::RecordedAction {
                        conn: a.conn,
                        player: a.player,
                        op: a.op,
                        payload: a.payload.clone(),
                    })
                    .collect(),
            )
        });
        self.logic.ingest(&mut self.world, &ctx, &mut actions);

        // -- Phase 2c — the requests no table rule claims: the logic's
        //    `handle_request` (an `External` decision becomes a built-in
        //    call of the table).
        parts!(self).handle(&requests, &mut *self.logic, &mut self.world, &ctx);
        // -- Phase 3 — SYSTEMS: run the ordered game systems, with the
        //    call table; then FLUSH: this tick's calls leave, in issue
        //    order (a refusal is decided here and delivered at the next
        //    RESPONSES).
        #[cfg(feature = "record")]
        self.calls.note(|| crate::determinism::Input::Systems);
        self.logic.update_with_events(
            &mut self.world,
            &ctx,
            &mut self.calls,
            &mut self.events.table,
        );
        self.calls.flush();

        // -- Phase 3b — KICK (E8): the kicks this tick's input and
        //    systems hooks asked for. The clock comes back first (the
        //    disconnect path takes a member off it), and is lent again
        //    for the broadcast.
        self.idle = idle;
        self.paths = paths;
        self.apply_kicks(kicks.take());
        let idle = std::mem::take(&mut self.idle);
        let paths = std::mem::take(&mut self.paths);
        let ctx = TickCtx {
            room: self.config.id,
            tick: t.tick,
            dt,
            time: self.clock,
            rng: rng.view(),
            idle: crate::room::IdleView::new(&idle, t.at),
            kicks: kicks.kicks(),
            paths: paths.view(self.config.period()),
        };

        // -- Phase 4 — BROADCAST: one snapshot per group, frozen once and
        //    shared by reference; per-connection fan-out of
        //    [group snapshot] + [private?].
        //    (the step counter was bumped at the top of `step`)
        //    First the call table closes what nobody took or answered
        //    this tick (the engine's answers ride this broadcast).
        #[cfg(feature = "record")]
        self.calls.note(|| crate::determinism::Input::EndTick);
        parts!(self).end_tick();
        self.events.end_tick();
        self.broadcast_phase(&ctx);
        // The lend is over (NLL ends `ctx`'s borrow at its last use);
        // hand the clock back to the actor, then apply what the
        // broadcast-phase hooks kicked.
        self.idle = idle;
        self.rng = rng;
        self.paths = paths;
        self.apply_kicks(kicks.take());
        // The step's input record (feature `record`; a no-op unless the
        // room records).
        #[cfg(feature = "record")]
        self.finish_record(t.tick, dt);
        true
    }
}
