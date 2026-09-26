//! The five-phase tick body. Each phase that carries real weight is a
//! child module; this file is the order they run in.

use crate::room::*;
use crate::ticker::TickInfo;
use prost::Message;
use std::fmt::Debug;
use std::hash::Hash;
use tracing::warn;

use crate::room::actor::RoomActor;

mod completions;
mod detach;
mod idle;
mod read;
mod requests;

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

        let mut actions = self.phase_read(t.at);

        // -- The tick context is built HERE, after every phase that
        //    writes the idle clock has run, because it LENDS that clock to
        //    the logic (`ctx.since_input`). The clock is moved out of the
        //    actor for the duration of the body: the phases below take
        //    `&mut self`, which a borrow living inside `ctx` would
        //    forbid. The move is an O(1) pointer swap, nothing between
        //    here and the restore reads or writes the clock, and no phase
        //    below this point returns early — so the restore is
        //    unconditional.
        let idle = std::mem::take(&mut self.idle);
        let ctx = TickCtx {
            room: self.config.id,
            tick: t.tick,
            dt,
            idle: crate::room::IdleView::new(&idle, t.at),
        };
        // -- Phase 2a — split the requests out of the pulled actions (the
        //    RPC pattern, see `crate::rpc`). A request is an action
        //    carrying the base-band envelope opcode; the core decodes the
        //    envelope (a base message) and hands the rest to the logic.
        //    The split is in-order: both the actions and the requests
        //    keep their arrival order (the processing order contract:
        //    all actions, then all requests — module docs of `rpc`).
        //
        //    Cost when quiet: one `u16` compare per pulled action — the
        //    common case (no requests in the tick) touches nothing else.
        //
        //    A malformed envelope is a *normal rejection* (a client bug —
        //    the same class as an undecodable game payload, which the
        //    logic ignores today), not a protocol violation: the room
        //    answers with a reject reply and counts it. `id = 0` is
        //    rejected the same way (it cannot correlate).
        let mut requests: Vec<crate::rpc::RpcRequest> = Vec::new();
        actions.retain_mut(|a| {
            if a.op != crate::rpc::RPC_REQ_OP {
                return true;
            }
            match gsb_protocol::base::RpcRequest::decode(&a.payload[..]) {
                Ok(env) if env.id != 0 => {
                    requests.push(crate::rpc::RpcRequest {
                        conn: a.conn,
                        // Already translated (phase 1.5): the logic sees
                        // the stable player key, while pending/replies
                        // stay session-scoped under `conn`.
                        player: a.player,
                        id: env.id,
                        // The wire type is `u32` (proto3 has no 16-bit
                        // integers); the op space is `u16` by protocol
                        // contract — the same range the op registry
                        // applies to every opcode (a value above the
                        // space decodes to the `u16` truncation, and the
                        // logic simply sees an op it does not handle).
                        op: env.op as u16,
                        payload: env.payload.into(),
                    });
                    false
                }
                // Its answer would be owed too: a congested connection at
                // its cap is refused here, like at 2c (F14).
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

        // -- Phase 2b — CONVERT: actions → component writes (game logic).
        //    All fire-and-forget actions are processed before the
        //    requests (the ordering contract; a request sees the world
        //    after this tick's actions were applied).
        self.logic.ingest(&mut self.world, &ctx, &mut actions);

        self.phase_requests(&requests, &ctx);
        // -- Phase 3 — SYSTEMS: run the ordered game systems.
        self.logic.update(&mut self.world, &ctx);

        // -- Phase 4 — BROADCAST: one snapshot per group, frozen once and
        //    shared by reference; per-connection fan-out of
        //    [group snapshot] + [private?].
        //    (the step counter was bumped at the top of `step`)
        self.broadcast_phase(&ctx);
        // The lend is over (NLL ends `ctx`'s borrow at its last use);
        // hand the clock back to the actor.
        self.idle = idle;
        true
    }
}
