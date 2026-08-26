//! The border exchange: what a shard publishes about its boundary
//! entities, the per-link full/delta ledger, and the view a neighbour
//! keeps of it.

use std::collections::HashMap;
use std::fmt::Debug;




/// One neighbor's boundary entity, as included in this shard's snapshots:
/// the CORE-MANAGED identity envelope (`wire` — minted from the room's
/// range-partitioned counters, deduplicated by the own-wins filter,
/// exited by the delta protocol) around a LOGIC-OWNED payload (`state`).
///
/// Why the split lives here: the seam's identity vocabulary (wire ids)
/// is the core's — every protocol mechanism keys off it — but WHAT an
/// entity must carry across the seam is the game's decision
/// (`docs/TRAIT-ARCHITECTURE.md`: state AND encoding belong to the
/// logic). A position-only game keeps the payload minimal; a combat or
/// prediction game extends it (velocity, facing, hp snapshot) without
/// touching this crate. Serialization at process-boundary links is the
/// future work of `docs/DISTRIBUTED.md` §4b — the codec will belong to
/// the logic because the payload type already does.
#[derive(Debug, Clone, PartialEq)]
pub struct BorderRecord<S> {
    /// The entity's wire identity (core vocabulary — see the module docs,
    /// "Wire identity").
    pub wire: u64,
    /// The game-defined strip payload, opaque to the core.
    pub state: S,
}

/// One neighbor's boundary update (CROSS-SHARD §6.4 pin 1–2): either the
/// COMPLETE strip (`Full` — bootstrap, resync, the periodic sigorta) or the
/// difference against what that neighbor last accepted from us (`Delta`:
/// upserts for new/changed records plus EXPLICIT exits for wire ids that
/// left the strip — without exits a ghost would persist forever, because a
/// delta carries no implicit "everything else is unchanged AND STILL THERE"
/// contract the way a full replacement does).
///
/// `seq` is the SENDER-side per-neighbor monotonic sequence (advanced once
/// per exchange actually queued): the receiver tracks the expected value
/// and treats any mismatch as a lost exchange, triggering a resync (pin 3a)
/// instead of silently diverging.
#[derive(Debug)]
pub enum BorderExchange<S> {
    /// The complete boundary strip: the receiver replaces its whole view
    /// for this neighbor and re-baselines its expected sequence. Accepted
    /// at ANY time — this is what makes a rebuilt shard's recovery
    /// automatic (its fresh incarnation restarts the sequence and always
    /// leads with a Full) without extra rebuild-notification wiring
    /// (§6.4 pin 3b).
    Full {
        seq: u64,
        /// The tick index the set was sampled at (diagnostics).
        tick: u64,
        entities: Vec<BorderRecord<S>>,
    },
    /// The difference against the receiver's last-known state. Applied
    /// atomically ONLY when `seq` matches the expected value exactly; any
    /// other value rejects the whole delta (never half-applied state) and
    /// requests a resync.
    Delta {
        seq: u64,
        /// See [`BorderExchange::Full::tick`].
        tick: u64,
        /// New or changed boundary records.
        upserts: Vec<BorderRecord<S>>,
        /// Wire ids that LEFT the strip since the last accepted exchange.
        exits: Vec<u64>,
    },
}

// measurement scaffolding for CROSS-SHARD §7 — remove or promote after
// the delta decision. These counters quantify the CURRENT full-state
// border exchange (bytes/records/CPU per tick, send drops) so a delta
// implementation can be judged against real numbers; the delta branch
// reuses this exact accounting for an apples-to-apples comparison.

/// The accounted payload size of ONE record: a u64 wire id plus the
/// payload's IN-MEMORY size. In process nothing is serialized, so this is
/// the wire-format LOWER bound a process-boundary deployment would pay
/// (the real codec is future work owned by the logic —
/// `docs/DISTRIBUTED.md` §4b); accounting through one helper keeps every
/// sender/receiver number comparable.
pub(crate) fn border_record_len<S>(r: &BorderRecord<S>) -> u64 {
    (std::mem::size_of::<u64>() + std::mem::size_of_val(&r.state)) as u64
}

/// The accounted payload size of one FULL [`BorderExchange`]: a u64 seq
/// header plus one record per entity. Both the baseline and the delta
/// implementation account through this helper (and [`delta_payload_len`]
/// for deltas) so the numbers stay comparable.
pub(crate) fn border_payload_len<'a, S: 'a>(
    records: impl Iterator<Item = &'a BorderRecord<S>>,
) -> u64 {
    std::mem::size_of::<u64>() as u64 + records.map(border_record_len).sum::<u64>()
}

/// The accounted payload size of one DELTA [`BorderExchange`]: the same
/// u64 seq header, one record per upsert and 8 bytes per exit (a bare
/// wire id). Same lower-bound accounting discipline as
/// [`border_payload_len`] — this is what a delta costs on a wire.
pub(crate) fn delta_payload_len<S>(upserts: &[BorderRecord<S>], exits: usize) -> u64 {
    std::mem::size_of::<u64>() as u64
        + upserts.iter().map(border_record_len).sum::<u64>()
        + (exits as u64) * std::mem::size_of::<u64>() as u64
}

/// The OWNED exchange payload computed before the send (phase 5): the
/// ledger commit on send success reads this instead of the queued
/// message, so no clone of the strip is kept alive for the commit.
/// Module-level because it is generic over the strip payload (a nested
/// item cannot see its parent's generics).
pub(crate) enum Commit<S> {
    Full,
    Delta {
        upserts: Vec<BorderRecord<S>>,
        exits: Vec<u64>,
    },
}

/// How often each neighbor is force-served a FULL exchange even when
/// deltas would do (§6.4 pin 3c): the low-frequency sigorta against
/// silent divergence — any bug that loses an untracked update heals at
/// the next cadence tick instead of never. 256 ticks ≈ 8.5 s at 30 Hz:
/// rare enough to be invisible in the byte budget, frequent enough to
/// bound the divergence lifetime far below any operational timescale.
pub(crate) const BORDER_FULL_EVERY_TICKS: u64 = 256;

/// Per-window border-exchange counters of ONE shard actor: send side =
/// phase 5 (collect_border + delta-vs-ledger + try_send to every
/// neighbor), receive side = the CONTROL-phase `ShardMsg::Border` arm
/// (apply upserts/exits or replace wholesale; there is no separate decode
/// step in process). Windowed on purpose: `step` resets the struct every
/// ~1 s of ticks and logs the deltas as one summary line — cumulative
/// counters would only give run-averages, while the decision needs
/// steady-state rates. The baseline fields keep their full-era semantics
/// (`exports`/`export_records`/`export_bytes` now account whatever is
/// ACTUALLY shipped — a delta's accounted size, not the strip's) so the
/// §7 baselines stay directly comparable; the fields below them are the
/// delta-era additions.
#[derive(Debug, Default)]
pub(crate) struct BorderStats {
    /// Exchanges sent (one per neighbor per tick that ships anything),
    /// cumulative-in-window: Fulls AND deltas AND failed attempts —
    /// attempts, because the drop counters need the same denominator as
    /// the full-era baseline.
    pub(crate) exports: u64,
    /// Records shipped, summed over all sends in-window (a Full counts
    /// its whole strip, a Delta its upserts+exits; counted per send,
    /// not per unique record, because that is what the channel carries).
    pub(crate) export_records: u64,
    /// Accounted payload bytes actually shipped ([`border_payload_len`]
    /// for Fulls, [`delta_payload_len`] for deltas) summed over all sends
    /// in-window.
    pub(crate) export_bytes: u64,
    /// Largest single export's record count in-window (context for the
    /// mean: one dense seam vs uniformly thin borders).
    pub(crate) export_records_max: usize,
    /// Wall time spent in phase 5 (collect + per-neighbor delta diff +
    /// clone + try_send), summed over ticks in-window (µs).
    pub(crate) export_us: u64,
    /// `try_send` failures against full/closed neighbor mailboxes,
    /// in-window (each one is a lost exchange; the delta path answers it
    /// with a forced Full on the next tick instead of waiting out the
    /// divergence until the periodic cadence).
    pub(crate) export_drops: u64,
    /// Exchanges received and applied, in-window (rejected deltas are NOT
    /// imports — they show up as `resync_requests_sent`).
    pub(crate) imports: u64,
    /// Records applied, in-window.
    pub(crate) import_records: u64,
    /// Accounted payload bytes applied, in-window.
    pub(crate) import_bytes: u64,
    /// Wall time spent applying received exchanges (map insert/remove /
    /// wholesale replace), summed over messages in-window (µs).
    pub(crate) import_us: u64,

    // -- Delta-era additions (CROSS-SHARD §6.4 / Faz 1). Additive by
    //    design: the baseline comparison needs the fields above intact. --
    /// Deltas queued successfully, in-window.
    pub(crate) delta_exchanges: u64,
    /// Fulls queued successfully, in-window (bootstrap + resync + the
    /// periodic cadence + send-failure recovery).
    pub(crate) full_exchanges: u64,
    /// Fulls served BECAUSE the neighbor asked for a resync, in-window.
    pub(crate) full_resyncs_served: u64,
    /// Resync requests SENT upstream after rejecting a delta (seq gap /
    /// stale view), in-window.
    pub(crate) resync_requests_sent: u64,
    /// Deltas LOST to `try_send` failures, in-window (the subset of
    /// `export_drops` that was a delta — each one is divergence until the
    /// forced Full lands).
    pub(crate) delta_drops: u64,
    /// What the equivalent FULL exchanges would have accounted, in-window
    /// (ledger-sized [`border_payload_len`] per successful send): the
    /// context number that makes "delta vs full" readable from one log
    /// line without re-running the baseline.
    pub(crate) equiv_full_bytes: u64,
}

/// The SENDER-side per-neighbor state of the delta protocol: what this
/// neighbor last accepted from us, under which sequence number, plus the
/// two flags that force the next exchange to be a Full. Keyed by shard
/// index like the mailbox table; created lazily on first export.
#[derive(Debug)]
pub(crate) struct NeighborExport<S> {
    /// The strip as THIS neighbor last accepted it (wire → record): the
    /// baseline every delta is diffed against. Advanced only on a
    /// successfully queued exchange — a Full overwrites it with the whole
    /// current strip, a Delta applies its own upserts/exits.
    pub(crate) ledger: HashMap<u64, BorderRecord<S>>,
    /// The sequence number stamped on the last exchange QUEUED for this
    /// neighbor (monotonic per sender incarnation; a fresh actor restarts
    /// at 0 and leads with a Full, which is exactly why a rebuilt shard
    /// resyncs its receivers with no extra wiring).
    pub(crate) seq: u64,
    /// Force the NEXT export to this neighbor to be a Full. Set by a send
    /// failure (a lost delta is divergence until healed — self-healing
    /// within ONE tick instead of waiting for the 256-tick cadence), by an
    /// explicit resync request, and by FIRST CONTACT; cleared by the Full
    /// that answers it.
    pub(crate) needs_full: bool,
    /// The neighbor explicitly asked for a resync
    /// ([`ShardMsg::ResyncRequest`]): the serving Full is counted as
    /// `full_resyncs_served`. Implies `needs_full`.
    pub(crate) resync_requested: bool,
}

impl<S> Default for NeighborExport<S> {
    /// A brand-new entry means UNKNOWN receiver state — first contact,
    /// or a freshly rebuilt incarnation meeting a receiver that still
    /// holds the dead incarnation's view. The protocol's answer to
    /// unknown is always the same: lead with a Full (`needs_full` starts
    /// TRUE, unlike a derived default).
    fn default() -> Self {
        Self {
            ledger: HashMap::new(),
            seq: 0,
            needs_full: true,
            resync_requested: false,
        }
    }
}

/// The RECEIVER-side per-neighbor state of the delta protocol: the
/// persistent borrowed view built incrementally from the neighbor's
/// exchanges, plus the continuity guard. Replaces the full-era
/// latest-whole-exchange slot.
#[derive(Debug)]
pub(crate) struct NeighborView<S> {
    /// The borrowed boundary records, keyed by wire id (upserts insert,
    /// exits remove — no ghosts survive an exit).
    pub(crate) recs: HashMap<u64, BorderRecord<S>>,
    /// The sequence number the NEXT delta from this neighbor must carry.
    /// A mismatch means an exchange went missing: reject, request a
    /// resync, stop trusting the view until a Full re-baselines it.
    pub(crate) expected_seq: u64,
    /// Set when a gap/stale-seq delta was rejected: the view MAY be wrong
    /// by an unknown amount (we know only THAT we lost something), so it
    /// is excluded from snapshots until the healing Full arrives —
    /// rendering possibly-diverged borrowed entities (ghost positions,
    /// despawned ids) would be worse than their brief absence.
    pub(crate) stale_until_full: bool,
}

impl<S> Default for NeighborView<S> {
    /// A fresh view trusts nothing yet: empty records, expecting sequence
    /// 0 (a Full re-baselines), not quarantined.
    fn default() -> Self {
        Self {
            recs: HashMap::new(),
            expected_seq: 0,
            stale_until_full: false,
        }
    }
}
