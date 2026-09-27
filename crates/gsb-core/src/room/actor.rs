//! The room actor's state. Its behaviour is split across child
//! modules by tick phase and by control path; they are children, so
//! the world and the tables stay private to this module tree.

use crate::channel::{Inbox, Mailbox};
use crate::id::{ConnectionId, PlayerId};
use crate::metrics::MetricsEvent;
use crate::room::*;
use crate::ticker::TickInfo;
use std::collections::HashMap;
use std::time::Instant;
use tokio::sync::{broadcast, mpsc};

mod control;
mod lifecycle;
mod snapshot;
mod tick;

/// The room actor. Owns the world, the player table (+ its session
/// binding table), and the group table; everything mutable is local, so
/// no synchronization is needed.
///
/// `G` is the game logic's group key ([`GameLogic::GroupKey`]) and `Sp`
/// its strip payload ([`GameLogic::Strip`], opaque here — the room never
/// exchanges borders); the room
/// stores per-group state (last snapshot, this tick's ship, diagnostics)
/// under the key.
pub struct RoomActor<W, G, Sp> {
    pub(in crate::room) config: RoomConfig,
    pub(in crate::room) world: W,
    pub(in crate::room) logic: Box<dyn RoomLogic<W, GroupKey = G, Strip = Sp>>,
    pub(in crate::room) tick_rx: broadcast::Receiver<TickInfo>,
    pub(in crate::room) control_rx: Inbox<RoomControl>,
    /// The room's members, keyed by their STABLE player identity (Faz 2,
    /// `docs/TRAIT-ARCHITECTURE.md` §5). The key survives resume and (for
    /// the shard sibling) migration, so a resume no longer re-keys this
    /// table at all — only the channel halves inside the row swap.
    pub(in crate::room) conns: HashMap<PlayerId, RoomConn<G>>,
    /// THE session binding table (Faz 2): transport session → player.
    /// ONE map owns the conn ↔ PlayerId context; established at
    /// join/resume, torn down at leave / detach-expire-despawn. Resume
    /// updates THIS table (plus the channel halves inside the `conns`
    /// row) instead of re-keying N tables — the §14.1 RebindKey pass
    /// shrank to exactly this move (see `rebind_session`).
    ///
    /// Also the ingest-side authority: every pulled action's `conn` is
    /// translated to a `PlayerId` through it before CONVERT; an unbound
    /// conn (an old session's stray frame after a resume) drops there.
    pub(in crate::room) binding: HashMap<ConnectionId, PlayerId>,
    /// READ-phase scan order: the member players in join order, kept
    /// exactly in sync with `conns` by the control path only
    /// ([`Self::roster_add`] on join, [`Self::roster_remove`] on leave /
    /// superseded rejoin). A `Vec` because the READ phase needs a
    /// deterministic order it can rotate: a `HashMap` iteration is
    /// arbitrary but fixed within a run, and under a sustained overload
    /// that fixed prefix consumes the whole room pull budget on every
    /// tick while the tail connections are never reached (their actions
    /// deferred forever — see the READ phase's rotation note).
    ///
    /// Keyed by `PlayerId` (Faz 2): stable across resume — a rebind does
    /// not touch the roster AT ALL, so the rotation cursor's meaning is
    /// trivially unaffected (the pre-Faz-2 in-place rename is gone).
    pub(in crate::room) roster: Vec<PlayerId>,
    /// Each player's index in `roster` — the bookkeeping that makes a
    /// membership removal a swap-remove plus one index fix instead of an
    /// O(n) scan. Control-path state only: never touched between ticks.
    pub(in crate::room) roster_pos: HashMap<PlayerId, usize>,
    /// Round-robin cursor over `roster`: each READ starts at
    /// `read_cursor % roster.len()` and advances past every connection
    /// examined. Kept as an absolute count (not a raw index) so
    /// membership changes between reads degrade to a shifted start
    /// offset, never an out-of-range index.
    pub(in crate::room) read_cursor: usize,
    /// The input-idle clock: when each member last sent an
    /// action-bearing frame (see [`crate::room::IdleView`] for the
    /// structural definition and for who is deliberately NOT on it).
    /// Non-generic so the tick context can lend it to the game logic
    /// without dragging `G` through every hook signature; stamped by the
    /// READ phase, started/stopped at the same funnels `binding` is.
    pub(in crate::room) idle: IdleClock,
    /// How many input-idle-ceiling warnings this room has emitted. The
    /// guard is `== 0`, so the answer is always 0 or 1: the ceiling is a
    /// standing property of the room, and a room that is shedding idle
    /// members sheds many — one line per member per rotation would bury
    /// the signal. A counter rather than a flag so the warn-ONCE contract
    /// is observable (and lockable) without going through a tracing
    /// subscriber, whose callsite-interest caching makes cross-test
    /// capture unreliable.
    pub(in crate::room) idle_ceiling_warns: u32,
    /// How many detach-hold-ceiling warnings this room has emitted
    /// ([`crate::room::RoomConfig::max_detach_hold`]) — 0 or 1, the
    /// warn-once rule above: a room whose logic keeps vetoing past the
    /// ceiling will do it for many holds.
    pub(in crate::room) detach_ceiling_warns: u32,
    pub(in crate::room) groups: HashMap<G, GroupState>,
    /// Number of global ticks between steps (1 = room rate == global rate).
    pub(in crate::room) run_every: u64,
    /// Wall-clock instant of the last step (for dt).
    pub(in crate::room) last_at: Option<Instant>,
    /// Room's own step counter (keep-alive cadence is in *room* steps, so a
    /// slower room keeps the same keep-alive rate in real time).
    pub(in crate::room) steps: u64,
    /// Every k-th step, unchanged groups re-send their last snapshot
    /// (`None` = keep-alive disabled).
    pub(in crate::room) keepalive_every: Option<u64>,
    /// Every k-th step, the room emits a metrics sample (ties the send
    /// cadence to the collector's report cadence — A2; 1 = every step).
    pub(in crate::room) metrics_every: u64,
    /// The room's tick budget in µs (one period): the histogram's overflow
    /// boundary (A1). Precomputed once (it is constant over the room's life).
    pub(in crate::room) budget_us: u64,
    /// Local metric counters (see [`RoomCounters`]).
    pub(in crate::room) m: RoomCounters,
    /// Outbound metrics path: a *bounded* channel mailbox (see
    /// [`crate::metrics`]); the room sends with the synchronous `try_send`,
    /// so it adds no await (drops are counted and harmless).
    pub(in crate::room) metrics: mpsc::Sender<MetricsEvent>,
    // -- RPC (deferred completion; see `crate::rpc`) ----------------------
    /// In-flight external requests, per TRANSPORT SESSION (Faz 2 decision,
    /// documented where the contract left latitude): `pending`/`queued`
    /// stay conn-keyed BY DESIGN — they are session-scoped RPC state that
    /// dies with the session. RECONNECT §11 fixes today's leave semantics
    /// ("pending drops at detach; late reports are silently discarded")
    /// and detach already clears both tables, so a resumed player never
    /// inherits the dead session's in-flight work; re-keying them to
    /// PlayerId would CHANGE that policy (a resume would carry pending
    /// requests across sessions) for zero gain. Listed in
    /// `rebind_session`'s enumeration as CHECKED, not missed.
    /// (FIFO: deadlines are non-decreasing per connection, so only the
    /// head can expire.) Owner of the pending state: the room is the
    /// single authority on "is this request still pending" (module docs
    /// of `crate::rpc`).
    pub(in crate::room) pending:
        HashMap<ConnectionId, std::collections::VecDeque<crate::rpc::PendingRequest>>,
    /// Total in-flight external requests (the room-wide cap).
    pub(in crate::room) pending_total: usize,
    /// This tick's queued RPC answers per transport session; drained in
    /// the broadcast phase (handed to the logic's `private`) and emptied
    /// — except the answers of a dropped batch, which go back here, in
    /// order, for the next tick (F14; `crate::rpc`, "Delivery").
    /// Cleared with the SESSION on leave/rejoin/detach (a stale answer to
    /// a gone session must not be delivered); conn-keyed for the same
    /// session-scope reason as `pending` above.
    pub(in crate::room) queued: HashMap<ConnectionId, Vec<crate::rpc::RpcReply>>,
    /// Per-tick scratch for the rare path of the broadcast's RPC-answer
    /// hand-off (a connection's removed `queued` entry, borrowed for the
    /// logic's `private` call). A field so its capacity survives across
    /// ticks; the quiet room never writes it.
    pub(in crate::room) replies_buf: Vec<crate::rpc::RpcReply>,
    /// Worker reports: the room's inbox for delegated-request outcomes
    /// (drained non-blockingly in the CONTROL phase — no new await).
    pub(in crate::room) completions: Inbox<crate::rpc::Completion>,
    /// The room's sender half of the completion channel, cloned to each
    /// spawned worker (bounded: a burst of completions cannot exceed the
    /// room-wide pending cap, which is the channel's capacity).
    pub(in crate::room) completions_tx: Mailbox<crate::rpc::Completion>,
    /// The room's match-result sink (the control plane's result seam; see
    /// [`RoomLogic::match_result`]): a bounded mailbox, sent to with the
    /// synchronous `try_send` on shutdown (no await, best effort).
    pub(in crate::room) result_sink: Option<Mailbox<crate::registry::MatchResult>>,
    /// The registry's mailbox, used for two messages: the report of a
    /// detach that ended in despawn
    /// ([`crate::registry::RegistryMsg::DetachDespawned`]) — the policy
    /// declining to park, or a hold running out — and the request to
    /// close a member's connection
    /// ([`crate::registry::RegistryMsg::CloseConn`], E6).
    /// `None` for a standalone room (the direct-drive test harnesses) — it
    /// then simply has no registry to tell.
    pub(in crate::room) registry: Option<Mailbox<crate::registry::RegistryMsg>>,
    /// Detach-despawn reports not yet accepted by the registry's mailbox.
    ///
    /// The report is a `try_send` (the tick body stays synchronous — the
    /// room's only await is `tick_rx.recv()`), so a momentarily full
    /// registry mailbox would otherwise DROP it — and a dropped report is
    /// the very leak this message exists to close. Un-sent ids wait here
    /// and are retried on later ticks instead. Bounded in practice by the
    /// detaches that end while the registry is saturated, and it drains as
    /// soon as the registry catches up.
    ///
    /// Written from BOTH producers — the CONTROL phase's `Detach::Despawn`
    /// arm and the phase-0c hold sweep — and flushed once, at the end of
    /// phase 0c. CONTROL runs first in the same tick, so a declined park
    /// is normally reported on the tick it happens.
    pub(in crate::room) despawn_reports: Vec<ConnectionId>,
    /// Close requests not yet accepted by the registry's mailbox
    /// ([`crate::registry::CloseRequest`], BACKLOG E6): the members whose
    /// membership the input-idle ceiling ended under
    /// `afk_action = Disconnect`. Same rules as `despawn_reports` — a
    /// FULL mailbox keeps them for the next tick, a CLOSED one drops
    /// them (`crate::registry::flush_close_requests`); written and
    /// flushed in phase 0d. Never written without a registry.
    pub(in crate::room) close_requests: Vec<crate::registry::CloseRequest>,
}
