//! Phase 4 — BROADCAST: one snapshot per group, encoded once and
//! shared by reference with every member of it.

use crate::id::{ConnectionId, PlayerId};
use crate::room::*;
use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::fmt::Debug;
use std::hash::Hash;
use tracing::warn;

use crate::room::actor::RoomActor;

impl<W, G, Sp> RoomActor<W, G, Sp>
where
    G: Eq + Hash + Clone + Debug,
    // The strip payload's trait bounds (`GameLogic::Strip`) — restated so
    // calls through the logic object type-check; the room itself never
    // touches the payloads.
    Sp: Debug + Clone + PartialEq + Send + 'static,
{
    /// Phase 4, in four passes. Field-level borrows keep the logic, the
    /// world, the connection table, and the group table independently
    /// accessible — disjoint fields, no synchronization.
    pub(in crate::room) fn broadcast_phase(&mut self, ctx: &TickCtx) {
        let snap_op = self.logic.snapshot_op();
        let priv_op = self.logic.private_op();

        // 4a. Recompute each player's group (a group may depend on the
        //     world, e.g. zones).
        for (player, rc) in self.conns.iter_mut() {
            rc.group = self.logic.group_of(&self.world, *player);
        }

        // 4b. Rebuild the group table. Membership churn (join/leave)
        //     shows up here as a different member set — the game logic's
        //     "no change" test in `snapshot` must account for it.
        let mut members: HashMap<G, Vec<PlayerId>> = HashMap::new();
        for (&player, rc) in &self.conns {
            members.entry(rc.group.clone()).or_default().push(player);
        } // Gauge for the per-step sample: the largest group this tick.
        self.m.step_max_group = members.values().map(Vec::len).max().unwrap_or(0) as u32;
        // Drop groups whose members all left (frees the cached snapshot).
        let gone: Vec<G> = self
            .groups
            .keys()
            .filter(|g| !members.contains_key(*g))
            .cloned()
            .collect();
        for g in gone {
            self.groups.remove(&g);
        }
        for (g, m) in members {
            match self.groups.entry(g) {
                Entry::Vacant(e) => {
                    e.insert(GroupState {
                        last: None,
                        sent: None,
                        never_emitted_warned: false,
                        size_warned: false,
                    });
                }
                Entry::Occupied(mut e) => {
                    let st = e.get_mut();
                    st.sent = None;
                    // The group existed on the previous tick too. If it
                    // has members but has still never emitted, its first
                    // tick's snapshot returned `false` although a fresh
                    // group's first tick is a membership change — a
                    // contract violation the room can detect precisely
                    // (a merely *quiet* group will not trigger this: it
                    // has emitted at least once).
                    if st.last.is_none() && !st.never_emitted_warned {
                        st.never_emitted_warned = true;
                        // Cold path (once per such group). `st`'s last use
                        // ended above, so the key can be borrowed (not
                        // cloned): tracing formats the field within the
                        // statement.
                        let group_key = e.key();
                        warn!(
                            room = %self.config.id,
                            ?group_key,
                            members = m.len(),
                            "snapshot group has members but has never emitted: \
                             RoomLogic::snapshot returned `false` on the \
                             group's first tick although a fresh group's \
                             first tick is a membership change and must \
                             emit; its members receive nothing except \
                             keep-alive re-sends of a cache that was never \
                             set. Check the logic's per-group bookkeeping \
                             (see RoomLogic::snapshot)."
                        );
                    }
                }
            }
        }

        // 4c. One snapshot per group: encode ONCE, share the result by
        //     reference (the scratch buffer is reused across groups and
        //     ticks; `split_to` hands the room a zero-copy `Bytes` view of
        //     its growing heap allocation — no per-group allocation churn).
        //
        //     Keep-alive (on the cadence tick, whether the group emitted
        //     this tick or not): full-snapshot logics keep the default
        //     behaviour (an unchanged group re-sends its cached snapshot —
        //     the unchanged group's cache is the very snapshot that was
        //     just encoded, and an active group's tick payload already is
        //     a fresh full, so re-sending `last` is bit-identical);
        //     delta-mode logics return `true` with a freshly encoded FULL
        //     that REPLACES this tick's payload — a client that lost the
        //     delta (or several) is healed within one keep-alive period
        //     whether its group is active or silent (see
        //     `GameLogic::keepalive`).
        let keep_due = self
            .keepalive_every
            .map(|every| self.steps.is_multiple_of(every))
            .unwrap_or(false);
        let mut buf = bytes::BytesMut::new();
        for (group, st) in self.groups.iter_mut() {
            buf.clear();
            // No boundary records on the single-room path: the borrowed
            // slice is a sharded-execution-only input (see
            // `GameLogic::snapshot`).
            let emitted = self
                .logic
                .snapshot(&mut self.world, ctx, group, &[], &mut buf);
            if emitted {
                if buf.len() > self.config.max_snapshot_bytes && !st.size_warned {
                    st.size_warned = true;
                    warn!(
                        room = %self.config.id,
                        ?group,
                        bytes = buf.len(),
                        max = self.config.max_snapshot_bytes,
                        "snapshot exceeds max_snapshot_bytes (rUDP MTU readiness)"
                    );
                }
                // Metrics: one encoded snapshot and its payload size.
                self.m.snapshots += 1;
                let n = buf.len() as u64;
                self.m.snap_bytes = self.m.snap_bytes.saturating_add(n);
                if n > self.m.snap_bytes_max as u64 {
                    self.m.snap_bytes_max = n as u32;
                }
                // Count every oversized emit (the warn above fires once per
                // group; this counts all of them — the MTU/AOI signal).
                if n > self.config.max_snapshot_bytes as u64 {
                    self.m.snap_overflows += 1;
                }
                let payload = buf.split_to(buf.len()).freeze();
                st.sent = Some(payload.clone());
                st.last = Some(payload);
            }
            // The keep-alive decision (only when there is a cached
            // snapshot): the logic may replace this tick's payload with a
            // freshly encoded one (a delta-mode full) or keep the default
            // (re-send `last` — bit-identical for unchanged groups).
            if keep_due && st.last.is_some() {
                if !emitted {
                    // An unchanged group: a keep-alive re-send happened.
                    self.m.keepalive_resends += 1;
                }
                buf.clear();
                if self
                    .logic
                    .keepalive(&mut self.world, ctx, group, st.last.as_ref(), &mut buf)
                {
                    // A freshly encoded payload (a delta-mode full): count
                    // it like any other encoded snapshot.
                    if buf.len() > self.config.max_snapshot_bytes && !st.size_warned {
                        st.size_warned = true;
                        warn!(
                            room = %self.config.id,
                            ?group,
                            bytes = buf.len(),
                            max = self.config.max_snapshot_bytes,
                            "snapshot exceeds max_snapshot_bytes (rUDP MTU readiness)"
                        );
                    }
                    self.m.snapshots += 1;
                    let n = buf.len() as u64;
                    self.m.snap_bytes = self.m.snap_bytes.saturating_add(n);
                    if n > self.m.snap_bytes_max as u64 {
                        self.m.snap_bytes_max = n as u32;
                    }
                    if n > self.config.max_snapshot_bytes as u64 {
                        self.m.snap_overflows += 1;
                    }
                    let payload = buf.split_to(buf.len()).freeze();
                    st.sent = Some(payload.clone());
                    st.last = Some(payload);
                } else {
                    st.sent = st.last.clone();
                }
            }
        }

        // 4c'. Overlap metric (see `RoomCounters::snap_records`): the
        //      payload is opaque to the core, so the number of encoded
        //      records can only come from the logic — polled exactly once
        //      per step, right after the phase that produced them.
        self.m.snap_records = self
            .m
            .snap_records
            .saturating_add(self.logic.encoded_records());

        // 4d. Per-connection fan-out: one batch per connection — the
        //     group's shared snapshot (Bytes refcount, never copied) plus
        //     the connection's private frame, when the logic has one.
        let mut dropped: u64 = 0;
        // The private-frame scratch is reused across connections (the
        // payload is split off, the capacity retained — no per-connection
        // per-tick allocation).
        let mut pbuf = bytes::BytesMut::new();
        // RPC answers are the rare case: in a quiet room (no in-flight
        // requests — the loadgen steady state) this is ONE `is_empty`
        // probe for the whole fan-out. The per-connection map probe runs
        // only on the ticks that actually owe an answer (measured: the
        // 500-probe-per-tick form added tens of microseconds to the tick
        // floor; the quiet path must stay O(1), like the 0b sweep above).
        let has_replies = !self.queued.is_empty();
        // The answers of this tick's dropped batches (F14), put back
        // after the sweep below. Empty on the quiet path: no allocation.
        let mut unsent: Vec<(ConnectionId, Vec<crate::rpc::RpcReply>)> = Vec::new();
        for (&player, rc) in self.conns.iter_mut() {
            // A detached (or bot-fed) row ships nothing: its outbound half
            // is dead (or has no human behind it). Skipping here — instead
            // of letting the `try_send` fail — is what keeps the room's
            // drop counter meaning "slow CLIENT" and nothing else (§7).
            // Its group snapshot is still encoded (the parked entity is in
            // the world and the other members must see it); only THIS
            // row's fan-out is skipped.
            if rc.detached {
                continue;
            }
            // The batch buffer is reused across ticks (floor breakdown: the
            // per-tick `Vec::with_capacity(2)` was a measured slice). It is
            // handed to the channel with `mem::take` — zero allocation,
            // the retained capacity is what makes the reuse free — and, if
            // the outbound channel is full, put back for the next tick.
            rc.batch.clear();
            // Whether the group frame rides this batch: what a drop of it
            // costs the client is the logic's to judge (F11).
            let mut with_snapshot = false;
            if let Some(payload) = self.groups.get(&rc.group).and_then(|st| st.sent.clone()) {
                with_snapshot = true;
                // Metrics: one shipped frame and its wire payload size.
                self.m.shipped_frames += 1;
                self.m.shipped_bytes = self.m.shipped_bytes.saturating_add(payload.len() as u64);
                rc.batch
                    .push(gsb_protocol::FrameBody::new(snap_op, payload));
            }
            // This connection's queued RPC answers for the tick (empty for
            // the common case — the `has_replies` guard above keeps the
            // quiet fan-out free of per-connection map probes). The logic
            // encodes them into the private frame alongside any ack /
            // one-shot full.
            let replies: &[crate::rpc::RpcReply] = if has_replies {
                self.replies_buf = self.queued.remove(&rc.conn).unwrap_or_default();
                &self.replies_buf
            } else {
                &[]
            };
            pbuf.clear();
            if self
                .logic
                .private(&mut self.world, player, &rc.group, replies, &mut pbuf)
            {
                // Metrics: one shipped private frame and its payload size.
                self.m.private_frames += 1;
                self.m.shipped_frames += 1;
                self.m.shipped_bytes = self.m.shipped_bytes.saturating_add(pbuf.len() as u64);
                rc.batch.push(gsb_protocol::FrameBody::new(
                    priv_op,
                    pbuf.split_to(pbuf.len()).freeze(),
                ));
            }
            if !rc.batch.is_empty() {
                let batch = std::mem::take(&mut rc.batch);
                if let Err(e) = rc.out.try_send(batch) {
                    // Outbound channel full: the batch is dropped, never
                    // retried (the fan-out stays best-effort). A full
                    // snapshot costs the client one snapshot of staleness
                    // (keep-alive bounds it); what the batch's one-shot
                    // content costs is the logic's, so it is told —
                    // synchronously, in this player's iteration, while the
                    // state its `private` just derived is still current
                    // (F11). The buffer goes back for the next tick.
                    dropped += 1;
                    rc.batch = e.into_inner();
                    rc.dropping = true;
                    // The RPC answers the batch carried are the core's
                    // own (F14): they left `queued` above, so putting
                    // them back keeps them exactly-once, and they ride
                    // the next accepted batch ahead of any later answer.
                    // Only when this iteration filled the scratch — a
                    // quiet tick leaves the last delivered answers in it.
                    if has_replies && !self.replies_buf.is_empty() {
                        unsent.push((rc.conn, std::mem::take(&mut self.replies_buf)));
                    }
                    self.logic
                        .on_batch_dropped(&mut self.world, player, with_snapshot);
                } else if rc.dropping {
                    // The first batch through after a run of drops: the
                    // logic may release what it paced (F11).
                    rc.dropping = false;
                    self.logic.on_batch_resumed(&mut self.world, player);
                }
            }
        }
        // Every connection was visited above (each owns at most one
        // queued entry this tick), so anything left here belongs to a
        // connection that was removed from the table this same tick and
        // never got its frame: drop it (the request is answered exactly
        // once — it was never delivered).
        if !self.queued.is_empty() {
            self.queued.clear();
        }
        // The undelivered answers go back to the front of their (now
        // empty) queue; answers queued later append behind them.
        for (conn, replies) in unsent {
            self.queued.insert(conn, replies);
        }
        self.m.dropped_frames += dropped;
    }

    /// Queue one RPC answer for a connection's next (or this tick's, if
    /// broadcast has not run yet) private frame. All request paths —
    /// same-tick reply/reject, cap/duplicate rejects, the worker-report
    /// reconciliation, the timeout sweep — funnel through here, so the
    /// per-tick delivery point is exactly one.
    pub(super) fn queue_reply(
        &mut self,
        conn: ConnectionId,
        id: u64,
        op: u16,
        ok: bool,
        reason: String,
        payload: bytes::Bytes,
    ) {
        self.queued
            .entry(conn)
            .or_default()
            .push(crate::rpc::RpcReply {
                id,
                ok,
                op,
                reason,
                payload,
            });
    }

    /// Drop a connection's request state (pending set + queued answers).
    /// Called on leave and on join (a join supersedes the connection's
    /// prior state, including a queued leave). Late worker reports for
    /// the dropped requests find no pending entry and are dropped (the
    /// exactly-one-answer reconciliation); the workers themselves exit
    /// on their own (report send against a dropped entry, or their
    /// timeout).
    pub(super) fn drop_conn_request_state(&mut self, conn: ConnectionId) {
        if let Some(deq) = self.pending.remove(&conn) {
            self.pending_total = self.pending_total.saturating_sub(deq.len());
        }
        self.queued.remove(&conn);
    }
}
