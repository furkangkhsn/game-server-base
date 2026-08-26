//! Phase 5 — BORDER: the per-link export, full or delta.

use std::collections::HashMap;
use std::fmt::Debug;
use std::hash::Hash;
use std::time::Instant;

use tracing::warn;

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
    /// Tick phase 5 — BORDER: export this shard's boundary entities to
    /// each neighbour, full or delta per link.
    pub(crate) fn phase_border(&mut self, t: &TickInfo) {
    // -- Phase 5 — BORDER (export this shard's boundary entities to
    //    every neighbor as a §6.4 delta exchange: a Full against the
    //    per-neighbor ledger when continuity is broken or the periodic
    //    cadence fires, otherwise just upserts+exits; a quiet strip
    //    ships NOTHING, which is the entire point of the delta).
    //
    //    measurement scaffolding for CROSS-SHARD §7 — remove or
    //    promote after the delta decision: the whole phase (collect +
    //    diff-vs-ledger + send) is timed and counted into `bstats`
    //    with the SAME helpers as the full-era baseline, so bytes/
    //    records/µs stay directly comparable. With no neighbors
    //    nothing is exchanged, so nothing is counted — an edge/
    //    unsharded topology pays no instrumentation cost beyond the
    //    emptiness check.
    let nb = self.logic.neighbors().to_vec();
    if !nb.is_empty() {
        let t5 = Instant::now();
        let records = self.logic.collect_border(&self.world);
        // The current strip as a wire-keyed map: the delta diff and
        // the ledger update both want membership tests, and duplicate
        // wires (a game bug if any) collapse deterministically to the
        // last record instead of corrupting the ledger bookkeeping.
        let current: HashMap<u64, BorderRecord<Sp>> =
            records.into_iter().map(|r| (r.wire, r)).collect();
        // Pin 3c: the low-frequency periodic Full — one comparison per
        // neighbor, taken once per tick.
        let periodic_full = t.tick.is_multiple_of(BORDER_FULL_EVERY_TICKS);
        let mut drops = 0u64;
        let mut delta_drops = 0u64;
        let mut shipped_records = 0u64;
        let mut shipped_bytes = 0u64;
        let mut records_max = 0usize;
        let mut delta_exchanges = 0u64;
        let mut full_exchanges = 0u64;
        let mut resyncs_served = 0u64;
        let mut equiv_full_bytes = 0u64;
        let mut attempts = 0u64;
        for b in &nb {
            // Faz C per-link derivation: the LINK's class decides the
            // packaging. InProc ⇒ AlwaysFull (bytes free over a move,
            // local CPU scarce — CROSS-SHARD §7 A/B); future Ipc/Net
            // links will declare Delta. The override vec is the test
            // lever.
            let link_mode = match &self.exchange_override {
                Some(modes) => modes[*b],
                None => self.links[*b].exchange_mode(),
            };
            let st = self.export.entry(*b).or_default();
            // A Full is forced by: the link class running AlwaysFull,
            // the periodic cadence (3c), a send failure on the last
            // exchange (self-heal in ONE tick), an explicit resync
            // request from the neighbor, or first contact
            // (`needs_full` defaults true on a fresh actor — which is
            // also the rebuilt-incarnation path).
            let force_full = link_mode == ExchangeMode::AlwaysFull
                || periodic_full
                || st.needs_full;
            let was_resync = st.resync_requested;
            st.seq += 1;
            let seq = st.seq;
            let tick = t.tick;
            // The delta payload is computed OWNED first so the ledger
            // commit on send success does not need to read the message
            // back: the exchange carries a clone of exactly these
            // upserts/exits (one small-allocation copy per send — the
            // honest price counted in phase 5).
            let (exchange, payload, recs_shipped, commit) = if force_full {
                (
                    BorderExchange::Full {
                        seq,
                        tick,
                        entities: current.values().cloned().collect(),
                    },
                    border_payload_len(current.values()),
                    current.len(),
                    Commit::Full,
                )
            } else {
                // The delta: upserts = records missing from or changed
                // against the ledger; exits = ledger ids gone from the
                // strip. Both directions are explicit so neither new
                // nor departed entities can be misread as "unchanged".
                let mut upserts = Vec::new();
                for r in current.values() {
                    match st.ledger.get(&r.wire) {
                        // Whole-payload equality IS the change test:
                        // any field difference ships an upsert. A looser
                        // equivalence (ignore-jitter) is the payload
                        // owner's business — implemented in its
                        // `PartialEq` or by quantizing at collection.
                        Some(prev) if *prev == *r => {}
                        _ => upserts.push(r.clone()),
                    }
                }
                let mut exits: Vec<u64> = st
                    .ledger
                    .keys()
                    .filter(|w| !current.contains_key(w))
                    .copied()
                    .collect();
                exits.sort_unstable();
                if upserts.is_empty() && exits.is_empty() {
                    // Nothing changed since this neighbor last
                    // accepted: ship nothing — this silent-tick skip
                    // IS the byte win being measured. The sequence
                    // number is NOT consumed (rolled back here):
                    // skipping a tick must not manufacture a gap the
                    // receiver would treat as loss.
                    st.seq -= 1;
                    continue;
                }
                (
                    BorderExchange::Delta {
                        seq,
                        tick,
                        upserts: upserts.clone(),
                        exits: exits.clone(),
                    },
                    delta_payload_len(&upserts, exits.len()),
                    upserts.len() + exits.len(),
                    Commit::Delta { upserts, exits },
                )
            };
            attempts += 1;
            match self.links[*b].send(ShardMsg::Border {
                from: self.index,
                exchange,
            }) {
                Ok(()) => {
                    // Commit: the neighbor WILL see this exchange (the
                    // bounded FIFO holds it until its CONTROL drain),
                    // so the ledger may advance to it.
                    match commit {
                        Commit::Full => {
                            let st = self.export.get_mut(b).expect("just inserted");
                            st.ledger = current.clone();
                            st.needs_full = false;
                            st.resync_requested = false;
                            full_exchanges += 1;
                            if was_resync {
                                resyncs_served += 1;
                            }
                        }
                        Commit::Delta { upserts, exits } => {
                            let st = self.export.get_mut(b).expect("just inserted");
                            for r in upserts {
                                st.ledger.insert(r.wire, r);
                            }
                            for w in exits {
                                st.ledger.remove(&w);
                            }
                            delta_exchanges += 1;
                        }
                    }
                    shipped_records += recs_shipped as u64;
                    shipped_bytes += payload;
                    equiv_full_bytes += border_payload_len(current.values());
                    records_max = records_max.max(recs_shipped);
                }
                Err(_) => {
                    // The exchange did NOT reach the neighbor's queue:
                    // its ledger and view stay where they were, but the
                    // seq already moved — a plain delta next tick would
                    // look like a gap to the receiver. Force a Full
                    // instead (the one-tick self-heal): it re-baselines
                    // ledger AND receiver in one message.
                    drops += 1;
                    if matches!(commit, Commit::Delta { .. }) {
                        delta_drops += 1;
                    }
                    let st = self.export.get_mut(b).expect("just inserted");
                    st.needs_full = true;
                    warn!(
                        room = %self.config.id,
                        shard = self.index,
                        neighbor = b,
                        "border send failed (neighbor channel full); \
                         that neighbor is flagged for a Full re-sync \
                         on the next tick"
                    );
                }
            }
        }
        let s = &mut self.bstats;
        // Exports counts EXCHANGES (queued or failed), not tick×neighbor
        // slots: a silently-skipped neighbor shipped nothing and must not
        // inflate the denominators the byte/record rates divide by.
        s.exports += attempts;
        s.export_records += shipped_records;
        s.export_bytes += shipped_bytes;
        s.export_records_max = s.export_records_max.max(records_max);
        s.export_us += t5.elapsed().as_micros() as u64;
        s.export_drops += drops;
        s.delta_exchanges += delta_exchanges;
        s.full_exchanges += full_exchanges;
        s.full_resyncs_served += resyncs_served;
        s.delta_drops += delta_drops;
        s.equiv_full_bytes += equiv_full_bytes;
    }

    }
}
