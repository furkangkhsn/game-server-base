//! The shard actor's state. Its behaviour is split across child
//! modules by tick phase and by message; they are children, so the
//! world and the tables stay private to this module tree.

use std::collections::{HashMap, VecDeque};
use std::time::Instant;

use tokio::sync::{broadcast, mpsc};

use crate::channel::{Inbox, Mailbox};
use crate::id::{ConnectionId, PlayerId};
use crate::metrics::MetricsEvent;
use crate::room::{GroupState, IdleClock, RoomConfig, RoomConn, RoomCounters};
use crate::rpc::{Completion, PendingRequest, RpcReply};
use crate::shard::*;
use crate::ticker::TickInfo;

mod lifecycle;
mod messages;
mod session;
mod snapshot;
mod tick;

/// The shard actor. Owns one shard's world, its player table (the
/// shard's share of the room's members), and its group table — the
/// same ownership discipline as the room actor (`RoomActor<W, G, Sp>`),
/// plus the shard protocol state (neighbor mailboxes, the delta
/// protocol's per-neighbor sender ledgers and receiver views, the
/// pending migrate-out marks, the deferred migrations, the conn-epoch
/// tables). `G` is the snapshot group key, `St` the migration state and
/// `Sp` the boundary-strip payload (all three game-owned; see
/// [`ShardLogic`] and [`GameLogic::Strip`](crate::room::GameLogic::Strip)).
pub struct ShardActor<W, G, St, Sp> {
    pub(in crate::shard) config: RoomConfig,
    pub(in crate::shard) index: usize,
    pub(in crate::shard) world: W,
    pub(in crate::shard) logic: Box<dyn ShardLogic<W, GroupKey = G, State = St, Strip = Sp>>,
    pub(in crate::shard) tick_rx: broadcast::Receiver<TickInfo>,
    /// The shard's own inbound link over the shared per-shard inbox
    /// (registry control AND neighbor protocol messages ride ONE bounded
    /// FIFO — registry.rs pass 1). Held as a [`ShardLink`] so the CONTROL
    /// drain crosses the seam: a future process-boundary deployment swaps
    /// the transport without touching the tick body.
    pub(in crate::shard) inbox: Box<dyn ShardLink<St, Sp>>,
    /// This shard's members, keyed by STABLE player identity (Faz 2 —
    /// same shape as the room actor; a resume or migration never re-keys
    /// this table).
    pub(in crate::shard) conns: HashMap<PlayerId, RoomConn<G>>,
    /// THE session binding table (Faz 2 — the shard-side twin of the
    /// room's): transport session → player. Established at join /
    /// migrate-in / resume; torn down at leave / detach-expire-despawn /
    /// migrate-out. Also the ingest-side authority translating every
    /// pulled action's `conn`.
    pub(in crate::shard) binding: HashMap<ConnectionId, PlayerId>,
    /// The epoch of the join this shard currently holds per TRANSPORT
    /// SESSION (set on Join/Migrate-in/resume; carried in outgoing
    /// Migrate messages). Conn-keyed BY DESIGN (Faz 2, documented where
    /// the contract left latitude): epochs are minted per session at the
    /// dispatcher, and the leave/migration race gate pairs a session's
    /// LEAVE against a session's in-flight MIGRATE — re-keying to stable
    /// players would make an old session's tombstone kill a resumed
    /// session's legitimate migration. A resume moves the entry as part
    /// of the binding move (the live SESSION changed); enumerated in
    /// `rebind_session`'s signpost.
    pub(in crate::shard) conn_epoch: HashMap<ConnectionId, u64>,
    /// The highest LEAVE epoch this shard has processed per connection —
    /// the leave/migration race gate: a Migrate of a join whose leave
    /// was already processed is dead and must be rejected. Kept SEPARATE
    /// from `conn_epoch` on purpose: an entry there can come from the
    /// join itself (or a migration install) — the join is alive, not
    /// dead — and gating on it would reject legitimate re-migrations of
    /// the same join (see module docs, "Migration protocol").
    ///
    /// Conn-keyed BY DESIGN (Faz 2): a tombstone guards the id of the
    /// join that DIED — a detached session never died (no leave was
    /// processed for it), so it wrote none; dead old sessions keep their
    /// guards under their own ids where they belong.
    ///
    /// The value is `(highest dead epoch, tick the winning leave was
    /// processed at)`: the tick half drives the TTL sweep. It is written
    /// by the leave that owns the surviving (highest) epoch, so expiry
    /// always measures the age of the guard that is actually
    /// load-bearing; an equal-or-stale leave keeps the older entry whole,
    /// which is the conservative direction (its guard lives longer).
    /// Entries expire after `TOMBSTONE_TTL_TICKS` — see module docs,
    /// "Tombstone lifetime".
    pub(in crate::shard) conn_tombstone: HashMap<ConnectionId, (u64, u64)>,
    /// The tick index of the last tombstone sweep (`None` = never run).
    /// The sweep runs lazily in CONTROL when `ctx.tick` has advanced
    /// `TOMBSTONE_SWEEP_EVERY_TICKS` past it — no timer task, no extra
    /// awaited source.
    pub(in crate::shard) last_tombstone_sweep: Option<u64>,
    /// The input-idle clock (the room actor's field, mirrored): when
    /// each member last sent an action-bearing frame. Started at
    /// join/migrate-in/resume, stopped at despawn/migrate-out/detach/AI
    /// handover, stamped by the READ phase.
    pub(in crate::shard) idle: IdleClock,
    /// How many input-idle-ceiling warnings this shard has emitted —
    /// always 0 or 1 (the room actor's warn-once rule and its rationale).
    pub(in crate::shard) idle_ceiling_warns: u32,
    /// How many detach-hold-ceiling warnings this shard has emitted —
    /// 0 or 1 (the room actor's `detach_ceiling_warns`, mirrored).
    pub(in crate::shard) detach_ceiling_warns: u32,
    pub(in crate::shard) groups: HashMap<G, GroupState>,
    /// One outbound link per shard index (used for the neighbors'
    /// indices) — [`InProcLink`] wrappers around exactly the mailboxes
    /// passed in, moved not cloned, so queue capacity, FIFO order and
    /// drop timing are the channel's, unchanged. Non-neighbor slots keep
    /// their dummy senders (wrapped, never sent to).
    pub(in crate::shard) links: Vec<Box<dyn ShardLink<St, Sp>>>,
    /// Test/override lever for Faz C's per-link derivation: when set,
    /// phase 5 uses these modes per neighbor index INSTEAD of the link's
    /// own class (production leaves this None — InProcLink declares
    /// AlwaysFull; future Ipc/Net links declare Delta). Crate-visible so
    /// delta-path unit tests can force Delta against the identical
    /// struct.
    pub(in crate::shard) exchange_override: Option<Vec<ExchangeMode>>,
    /// The borrowed boundary view per neighbor (the RECEIVER side of the
    /// delta protocol): persistent records built incrementally from
    /// Fulls/Deltas, with the expected-sequence guard per neighbor.
    pub(in crate::shard) border: HashMap<usize, NeighborView<Sp>>,
    /// The keys of `border`, ascending — the deterministic lookup order
    /// of the cross-seam view ([`CrossSeam`]). Grows once per neighbour
    /// on its first exchange; never shrinks (a neighbour is a topology
    /// fact).
    pub(in crate::shard) lenders: Vec<usize>,
    /// The remote-effect state (`docs/CROSS-SHARD.md` §2): the outbox
    /// the tick hooks emit into, received effects awaiting their apply
    /// tick, the retry buffer, the forwarding table, the per-origin
    /// duplicate windows. Every table bounded — see [`EffectBook`].
    pub(in crate::shard) effects: EffectBook,
    /// The SENDER side of the delta protocol: what each neighbor last
    /// accepted from us (ledger + seq + the force-Full flags). Created
    /// lazily on first export; a fresh actor starts empty, so a rebuilt
    /// shard's first exchange is always a Full.
    pub(in crate::shard) export: HashMap<usize, NeighborExport<Sp>>,
    /// Entities marked out by a successful `Migrate` send: (wire, the tick
    /// index at which the shard despawns them).
    pub(in crate::shard) pending_out: Vec<(u64, u64)>,
    /// Migrations that arrived early (install gate not open — see
    /// `handle_msg`): re-offered at the next tick's CONTROL, in send
    /// order.
    pub(in crate::shard) deferred: VecDeque<ShardMsg<St, Sp>>,
    pub(in crate::shard) run_every: u64,
    pub(in crate::shard) last_at: Option<Instant>,
    pub(in crate::shard) steps: u64,
    pub(in crate::shard) keepalive_every: Option<u64>,
    pub(in crate::shard) metrics_every: u64,
    pub(in crate::shard) budget_us: u64,
    pub(in crate::shard) m: RoomCounters,
    // measurement scaffolding for CROSS-SHARD §7 — remove or promote
    // after the delta decision (see `BorderStats`).
    /// Border-exchange counters accumulated since the last summary.
    pub(in crate::shard) bstats: BorderStats,
    /// Summary cadence in steps: `tick_hz` rounded — one window ≈ 1 s.
    pub(in crate::shard) border_every: u64,
    pub(in crate::shard) metrics: mpsc::Sender<MetricsEvent>,
    // -- Shard-RPC (the Faz 3 promotion; see `crate::rpc` and the module
    //    docs, "Shard-RPC and match-result"): byte-for-byte the room
    //    actor's state shape. -------------------------------------------
    /// In-flight external requests, per TRANSPORT SESSION (conn-keyed BY
    /// DESIGN — see the module docs: a resumed session must not inherit
    /// its dead session's in-flight work). FIFO per conn: deadlines are
    /// non-decreasing, so only the head can expire. This shard is the
    /// single authority on "is this request still pending" for the
    /// sessions it owns.
    pub(in crate::shard) pending: HashMap<ConnectionId, VecDeque<PendingRequest>>,
    /// Total in-flight external requests on this shard (the shard-wide
    /// cap; the config fields are shared with the room actor).
    pub(in crate::shard) pending_total: usize,
    /// This tick's queued RPC answers per transport session; drained in
    /// the broadcast phase (handed to the logic's `private`) and emptied.
    /// Cleared with the SESSION on leave/rejoin/detach/migrate-out.
    pub(in crate::shard) queued: HashMap<ConnectionId, Vec<RpcReply>>,
    /// Per-tick scratch for the rare path of the broadcast's RPC-answer
    /// hand-off; a field so its capacity survives across ticks.
    pub(in crate::shard) replies_buf: Vec<RpcReply>,
    /// Worker reports: this shard's inbox for delegated-request outcomes
    /// (drained non-blockingly in the 0b phase — no new await).
    pub(in crate::shard) completions: Inbox<Completion>,
    /// The sender half of the completion channel, cloned to each spawned
    /// worker (bounded at the shard-wide pending cap — a completion burst
    /// cannot exceed the number of in-flight workers).
    pub(in crate::shard) completions_tx: Mailbox<Completion>,
    /// This shard's match-result sink (the control plane's result seam;
    /// see [`GameLogic::match_result`]): a bounded mailbox, sent with the
    /// synchronous `try_send` on teardown (no await, best effort). Every
    /// shard of a logical room shares the registry's sink.
    pub(in crate::shard) result_sink: Option<Mailbox<crate::registry::MatchResult>>,
    /// The registry's mailbox, used for exactly one report: a detach that
    /// ended in despawn
    /// ([`crate::registry::RegistryMsg::DetachDespawned`]) — the policy
    /// declining to park, or a hold running out. `None` for a
    /// directly-driven shard (the test rigs) — no registry to tell.
    pub(in crate::shard) registry: Option<Mailbox<crate::registry::RegistryMsg>>,
    /// Detach-despawn reports not yet accepted by the registry's mailbox;
    /// retried on later ticks rather than dropped. Written by the
    /// `ShardMsg::Detach` handler and by the phase-0c hold sweep, flushed
    /// once at the end of phase 0c. See the room actor's field of the same
    /// name for why a dropped report would reopen the leak.
    pub(in crate::shard) despawn_reports: Vec<ConnectionId>,
    // -- The team exchange (`docs/CROSS-SHARD.md` §8b). ----------------
    /// The other shards' team records (one slot per source, replaced by
    /// every `TeamImport`, TTL-expired, merged per team): what the TEAMS
    /// phase hands the logic.
    pub(in crate::shard) teams: TeamImports,
    /// The last export the registry accepted listed something: the next
    /// EMPTY one is still sent — once — so the receivers clear this
    /// shard's slot now instead of at the TTL.
    pub(in crate::shard) team_sent: bool,
    /// Team-exchange counters, cumulative (the metrics sample's
    /// `team_*`).
    pub(in crate::shard) tstats: TeamStats,
    /// `tstats` as the last `team_exchange_summary` line saw them: the
    /// line's window is the difference.
    pub(in crate::shard) tstats_logged: TeamStats,
}
