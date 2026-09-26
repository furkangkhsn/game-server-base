//! Phase 6 — BROADCAST: the room's broadcast phase with the borrowed
//! border strip folded into each group's view.

use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::fmt::Debug;
use std::hash::Hash;

use tracing::warn;

use crate::id::PlayerId;
use crate::room::{GroupState, TickCtx};
use crate::rpc::RpcReply;

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
    /// The borrowed boundary set (module docs, "Boundary visibility"):
    /// the persistent per-neighbor views flattened. Built once per tick
    /// and handed to the TEAMS phase and to every group's snapshot.
    pub(crate) fn borrowed_view(&self) -> Vec<BorderRecord<Sp>> {
        // Sorted by wire so the payload order is deterministic. A
        // quarantined view (`stale_until_full` — a rejected delta means
        // an unknown-sized hole) is EXCLUDED: rendering possibly-diverged
        // borrowed entities would be worse than their brief absence; the
        // healing Full restores them within a tick or two.
        let mut borrowed: Vec<BorderRecord<Sp>> = Vec::new();
        for recs in self.border.values() {
            if !recs.stale_until_full {
                borrowed.extend(recs.recs.values().cloned());
            }
        }
        borrowed.sort_unstable_by_key(|r| r.wire);
        // Own records win over the neighbor's one-tick-stale borrowed copy
        // of an entity that just crossed into this shard (otherwise it
        // would appear twice in one snapshot under the same wire id):
        // filter the borrowed set against the own wires (binary search on
        // the sorted own list — the border set is small).
        let mut own = self.logic.own_wires(&self.world);
        own.sort_unstable();
        borrowed.retain(|r| own.binary_search(&r.wire).is_err());
        borrowed
    }

    /// Phase 6: the room's broadcast phase, with the borrowed boundary set
    /// (the latest exchange per neighbor, flattened and sorted by wire for
    /// deterministic payload order — [`Self::borrowed_view`]) folded into
    /// every group's snapshot.
    pub(crate) fn broadcast_phase(&mut self, ctx: &TickCtx, borrowed: &[BorderRecord<Sp>]) {
        let snap_op = self.logic.snapshot_op();
        let priv_op = self.logic.private_op();

        // 6a. Recompute each player's group.
        for (player, rc) in self.conns.iter_mut() {
            rc.group = self.logic.group_of(&self.world, *player);
        }

        // 6b. Rebuild the group table (same as the room's 4b).
        let mut members: HashMap<G, Vec<PlayerId>> = HashMap::new();
        for (&player, rc) in &self.conns {
            members.entry(rc.group.clone()).or_default().push(player);
        }
        self.m.step_max_group = members.values().map(Vec::len).max().unwrap_or(0) as u32;
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
                    if st.last.is_none() && !st.never_emitted_warned {
                        st.never_emitted_warned = true;
                        let group_key = e.key();
                        warn!(
                            room = %self.config.id,
                            shard = self.index,
                            ?group_key,
                            members = m.len(),
                            "snapshot group has members but has never emitted: \
                             ShardLogic::snapshot returned `false` on the \
                             group's first tick although a fresh group's \
                             first tick is a membership change and must \
                             emit (check the logic's per-group bookkeeping)"
                        );
                    }
                }
            }
        }

        // 6c. One snapshot per group: encode ONCE, freeze once, share —
        //     the room's 4c with the borrowed boundary set folded in and
        //     the scratch buffer reused across groups. The keep-alive
        //     machinery mirrors the room's semantics EXACTLY (the Faz 1
        //     `GameLogic::keepalive` promotion — the shard actor's first
        //     promoted capability, `docs/TRAIT-ARCHITECTURE.md` §4): on
        //     the cadence tick, whether the group emitted this tick or
        //     not, the logic decides what ships. Full-snapshot logics
        //     keep the default (an unchanged group re-sends its cached
        //     snapshot — bit-identical: an active group's `last` IS this
        //     tick's fresh full); a delta-mode logic returns `true` with
        //     a freshly encoded FULL that REPLACES this tick's payload,
        //     healing a client that lost one or more deltas within one
        //     keep-alive period whether its group is active or silent.
        let keep_due = self
            .keepalive_every
            .map(|every| self.steps.is_multiple_of(every))
            .unwrap_or(false);
        let mut buf = bytes::BytesMut::new();
        for (group, st) in self.groups.iter_mut() {
            buf.clear();
            let emitted = self
                .logic
                .snapshot(&mut self.world, ctx, group, borrowed, &mut buf);
            if emitted {
                if buf.len() > self.config.max_snapshot_bytes && !st.size_warned {
                    st.size_warned = true;
                    warn!(
                        room = %self.config.id,
                        shard = self.index,
                        bytes = buf.len(),
                        max = self.config.max_snapshot_bytes,
                        "snapshot exceeds max_snapshot_bytes (rUDP MTU \
                         readiness)"
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
            }
            // The keep-alive decision (only when there is a cached
            // snapshot): the logic may replace this tick's payload with a
            // freshly encoded one (a delta-mode full) or keep the default
            // (re-send `last`). Same shape, same counters, same cadence
            // derivation (`keepalive_every`, clamped/warned at
            // construction exactly like the room actor's).
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
                    // A freshly encoded payload (a delta-mode full):
                    // counted like any other encoded snapshot.
                    if buf.len() > self.config.max_snapshot_bytes && !st.size_warned {
                        st.size_warned = true;
                        warn!(
                            room = %self.config.id,
                            shard = self.index,
                            bytes = buf.len(),
                            max = self.config.max_snapshot_bytes,
                            "snapshot exceeds max_snapshot_bytes (rUDP MTU \
                             readiness)"
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

        self.m.snap_records = self
            .m
            .snap_records
            .saturating_add(self.logic.encoded_records());

        // 6d. Per-connection fan-out (same as the room's 4d).
        let mut dropped: u64 = 0;
        // Same batch-buffer reuse as the room's 4d (one floor slice was the
        // per-connection per-tick `Vec::with_capacity(2)`).
        let mut pbuf = bytes::BytesMut::new();
        // RPC answers are the rare case (the room's measured rule): in a
        // quiet shard this is ONE `is_empty` probe for the whole fan-out;
        // the per-connection map probe runs only on ticks that actually
        // owe an answer.
        let has_replies = !self.queued.is_empty();
        for (&player, rc) in self.conns.iter_mut() {
            // Detached/bot-fed rows ship nothing (dead or non-human
            // outbound half; §7 — the drop counter stays "slow client"
            // only). The group snapshot still carries the parked entity.
            if rc.detached {
                continue;
            }
            rc.batch.clear();
            // Whether the group frame rides this batch (the drop signal's
            // argument — the room's 4d).
            let mut with_snapshot = false;
            if let Some(payload) = self.groups.get(&rc.group).and_then(|st| st.sent.clone()) {
                with_snapshot = true;
                self.m.shipped_frames += 1;
                self.m.shipped_bytes = self.m.shipped_bytes.saturating_add(payload.len() as u64);
                rc.batch
                    .push(gsb_protocol::FrameBody::new(snap_op, payload));
            }
            // This connection's queued RPC answers for the tick (Faz 3:
            // same-tick local replies and, on later ticks, the reconciled
            // worker reports / timeout sweeps). The logic encodes them
            // into the private frame alongside any ack / one-shot full.
            let replies: &[RpcReply] = if has_replies {
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
                    // Dropped, never retried; the logic is told in this
                    // player's iteration (the room's 4d, F11).
                    dropped += 1;
                    rc.batch = e.into_inner();
                    self.logic
                        .on_batch_dropped(&mut self.world, player, with_snapshot);
                }
            }
        }
        // Every connection was visited above, so anything left here
        // belongs to a connection removed from the table this same tick
        // (leave/migrate) that never got its frame: drop it — a request is
        // answered exactly once, and it was never delivered.
        if !self.queued.is_empty() {
            self.queued.clear();
        }
        self.m.dropped_frames += dropped;
    }
}
