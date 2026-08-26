//! The game-logic seam. One supertrait pair: [`GameLogic`] is the
//! contract every room shape shares (single or sharded), [`RoomLogic`]
//! marks the single-world rooms.
//!
//! NOT split further: a trait is one item, so its 360 lines cannot be
//! divided across files without inventing traits the design does not
//! have.

use std::fmt::Debug;
use std::hash::Hash;
use crate::id::{ConnectionId, EntityId, PlayerId};
use crate::room::*;


/// Game-side behaviour shared by every actor shape — the single source of
/// the contract that used to be duplicated between the room and the shard
/// (~17 near-identical methods; see `docs/TRAIT-ARCHITECTURE.md` §3): the
/// tick seam ([`Self::ingest`] / [`Self::update`] / [`Self::snapshot`]),
/// the snapshot-group partitioning, the membership hooks, and the
/// reconnect surface (detach/resume). Two thin subtraits add only their
/// actor's exclusive hooks: [`RoomLogic`] the room's request/result seams,
/// [`ShardLogic`](crate::shard::ShardLogic) the migration/topology hooks.
///
/// Implemented by the game crate; the core never inspects the world `W`
/// or the group key `GroupKey`.
pub trait GameLogic<W>: Send {
    /// Opaque key partitioning the room's connections into snapshot groups.
    /// `()` = one per room (everyone sees the whole world);
    /// `ConnectionId` = one snapshot per connection; anything else (e.g. a
    /// zone id) is a legitimate future grouping. `Debug` so the room's
    /// group diagnostics can name a misbehaving group.
    type GroupKey: Eq + Hash + Clone + Debug;

    /// What an entity carries across a SHARD boundary (the
    /// visibility-strip payload inside [`shard::BorderRecord`]). Lives on
    /// THIS supertrait because [`Self::snapshot`] is the single encode
    /// seam both actors share — the borrowed set reaches the encoder
    /// typed, so the payload type must be visible here too.
    ///
    /// Ownership follows the architecture rule (`docs/TRAIT-ARCHITECTURE.md`):
    /// the wire identity (`wire`) is core-managed, but WHAT travels beside
    /// it — position only, or velocity/facing/hp for combat/prediction
    /// games — is the game's decision, exactly like the migration
    /// [`shard::ShardLogic::State`]. Any logic that never runs sharded
    /// picks `()` and never sees a record. Serialization at
    /// process-boundary links stays future work owned by the logic
    /// (`docs/DISTRIBUTED.md` §4b). `PartialEq` is load-bearing on the
    /// sharded path: it IS the delta upsert test.
    type Strip: Debug + Clone + PartialEq + Send + 'static;

    /// Opcode under which the room ships group snapshots.
    fn snapshot_op(&self) -> u16;
    /// Opcode under which the room ships the per-connection private frame
    /// produced by [`Self::private`].
    fn private_op(&self) -> u16;

    /// Which snapshot group the player belongs to. Re-evaluated every
    /// tick: a group may depend on the world (e.g. the zone an entity is
    /// in). Keyed by the stable [`PlayerId`] (Faz 2) — a resumed session
    /// keeps its group without any re-keying.
    fn group_of(&self, world: &W, player: PlayerId) -> Self::GroupKey;

    /// Encode the group's snapshot payload into `out`.
    ///
    /// Return `false` when the group is unchanged since **this group's**
    /// last emitted snapshot — and "no change" **includes** membership
    /// (join/leave). The room then ships nothing to the group, except on a
    /// keep-alive tick (see [`Self::keepalive`]).
    ///
    /// The payload format is the logic's protocol decision:
    /// - a **full, self-contained** snapshot (the default, what
    ///   `all`/`team`/`pvs` and the shard rooms ship): no delta, no
    ///   history — the payload alone defines the group's entire world, and
    ///   a lost packet is healed by the next one;
    /// - a **full/delta stream** (the spatial AOI ships this): the payload
    ///   is marked full or delta on the wire, deltas apply on top of the
    ///   client's last accepted payload, and the logic must guarantee the
    ///   client can always get a full again: every fresh group member
    ///   receives a one-shot full via [`Self::private`], and every
    ///   keep-alive tick ships a fresh full via [`Self::keepalive`] (so a
    ///   client that lost one or more deltas heals within one keep-alive
    ///   period). The sequence number (global tick index) in the payload
    ///   is the client's loss detector.
    ///
    /// **Bookkeeping must be per-group.** The room calls this once per
    /// existing group, per tick, in *unspecified* order (a `HashMap`
    /// iteration, stable within a run but not to be depended on). Your
    /// "unchanged?" decision and your last-emitted bookkeeping must
    /// therefore be keyed by `group`: one call must not change another
    /// group's answer in the same tick. A single shared ledger is only
    /// correct for one-group rooms (`GroupKey = ()`) — the demo's `last`
    /// field is exactly that. With several groups, the group visited first
    /// consumes the change and rewrites the shared ledger, and every group
    /// visited afterwards sees "no change" for the rest of the run: their
    /// members starve (they receive only keep-alive re-sends of a cache
    /// that is stale from the start, or of nothing at all), and the room
    /// cannot detect it — silence is also the legitimate state of a
    /// genuinely unchanged group.
    ///
    /// `borrowed` carries the neighboring shards' boundary records on the
    /// SHARDED execution path (the shard actor folds its latest border
    /// exchange in — see `crate::shard`); a single-room actor passes an
    /// empty slice. The parameter lives here — on the shared supertrait,
    /// not on the shard subtrait — so ONE method serves both actors and
    /// the fan-out machinery stays textually identical: a plain room's
    /// "no change" test simply never sees borrowed content. Each record's
    /// payload is the game's own [`Self::Strip`] type, so encoding it is
    /// fully in the logic's hands.
    fn snapshot(
        &mut self,
        world: &mut W,
        ctx: &TickCtx,
        group: &Self::GroupKey,
        borrowed: &[crate::shard::BorderRecord<Self::Strip>],
        out: &mut bytes::BytesMut,
    ) -> bool;

    /// Encode a per-connection private frame (delivered only to this
    /// player's current session, alongside the group snapshot).
    ///
    /// The player's group is passed in: the room re-evaluates it every
    /// tick (see [`Self::group_of`]) and hands the current value over, so a
    /// logic that needs "which group is this player in" must not
    /// re-derive it — each re-derivation is extra table lookups per
    /// player per tick (measured: part of the idle floor).
    ///
    /// `responses` is this tick's list of RPC answers owed to `conn`
    /// (empty = none; see [`RoomLogic::handle_request`] and
    /// `crate::rpc`):
    /// same-tick local answers and, on later ticks, the deferred answers
    /// of external requests that completed (or timed out) since the
    /// request was processed. The logic encodes them into the private
    /// frame (`Private.responses` in the demo protocol) — the frame is
    /// emitted whenever there is ANY content (ack, one-shot full, or
    /// responses).
    ///
    /// Default: none.
    fn private(
        &mut self,
        _world: &mut W,
        _player: PlayerId,
        _group: &Self::GroupKey,
        _responses: &[crate::rpc::RpcReply],
        _out: &mut bytes::BytesMut,
    ) -> bool {
        false
    }

    /// Produce the payload to ship to a group on a **keep-alive tick**
    /// (the cadence is due). Called after `snapshot` for the same tick,
    /// whether it emitted a payload or the group was unchanged; on a
    /// keep-alive tick this method decides what actually goes out.
    ///
    /// The default re-sends the group's last cached snapshot (`last`) —
    /// correct for full, self-contained logics: for an unchanged group it
    /// is the very snapshot that healed a lost packet, and for an active
    /// group `last` *is* this tick's fresh full (the method was just
    /// called after `snapshot` set it), so re-sending is bit-identical to
    /// keeping the tick's payload. Return `false` to keep that behaviour.
    ///
    /// A delta-mode logic must return `true` with a freshly encoded
    /// **full** snapshot in `out` instead: re-sending the last *delta* is
    /// meaningless (a client that missed it has no baseline to apply it
    /// against; a current client would double-apply it), while a fresh
    /// full — shipped on the cadence tick whether the group is active or
    /// silent, replacing an active group's tick delta (a superset of it)
    /// — heals any client that lost one or more deltas, bounding the
    /// recovery time to the keep-alive period.
    fn keepalive(
        &mut self,
        _world: &mut W,
        _ctx: &TickCtx,
        _group: &Self::GroupKey,
        _last: Option<&bytes::Bytes>,
        _out: &mut bytes::BytesMut,
    ) -> bool {
        false
    }

    /// A player entered the room: mint (or restore) its stable
    /// [`PlayerId`], create its entity, and hand BOTH back — `player`
    /// keys every internal table from here on, `entity` is the wire id
    /// the join reply carries. The identity policy is the game's: core
    /// never invents player ids. A FRESH join mints a fresh id; the park
    /// ledger makes an id stable across resume for the same identity
    /// (the record rides the player state, §14.2).
    fn on_join(&mut self, world: &mut W, conn: ConnectionId) -> Admission;

    /// A player left the room: remove its entity (and any per-player
    /// bookkeeping). Keyed by the stable [`PlayerId`] (Faz 2): a leave of
    /// ANY session of this player lands here under the same key.
    fn on_leave(&mut self, world: &mut W, player: PlayerId);

    /// The connection's transport died: decide the parked entity's fate
    /// (`docs/RECONNECT.md` §3.1). Called once per disconnect, from the
    /// tick's CONTROL phase, INSTEAD of the `on_leave` a transport death
    /// used to trigger — a [`Detach::Hold`] answer keeps the entity, its
    /// world state, its group membership, AND the room-cap slot it
    /// occupies (§4: a parked player holds their slot).
    ///
    /// The logic records the park entry here (identity → entity + hold
    /// metadata) in WHATEVER storage it owns; per §14.2 that storage must
    /// be part of the migrating player state for sharded rooms, so a
    /// migration carries the park record along. Storage lives in the
    /// game state; policy and lookup live in this trait.
    ///
    /// `identity` is the resume key. On the ticket-auth path it is
    /// `ValidatedTicket.player`; on the local-auth path it is
    /// `Auth.name`, which makes resume demo/testing-only there (no
    /// cryptographic identity behind the name — noted at the ledger
    /// site by design).
    ///
    /// Default: [`Detach::Despawn`] — every pre-reconnect logic keeps
    /// today's behavior exactly, unchanged.
    fn on_disconnect(
        &mut self,
        _world: &mut W,
        _player: PlayerId,
        _identity: &str,
    ) -> Detach {
        Detach::Despawn
    }

    /// May the hold end NOW? Asked every CONTROL phase while a detached
    /// player is held WITHOUT a grace deadline (combat-held): the
    /// logic answers "no" while the hold must persist (an enemy nearby),
    /// "yes" to release. A veto extends the hold; the ceiling against an
    /// endless veto is the policy choosing `Hold { grace: Some(_) }`
    /// (§14.4: with a grace, the core's own timer ends the hold and this
    /// method is not consulted for timed holds). Cost note: only the
    /// (rarely populated) detached set is asked, per §14.4.
    ///
    /// Default: `true` (no logic veto — the hold ends at once, which for
    /// a `grace = None` hold means "expire immediately": logics that
    /// never want a hold simply return `Detach::Despawn` instead).
    fn may_release(&mut self, _world: &mut W, _player: PlayerId) -> bool {
        true
    }

    /// The hold ended without a resume (grace expired, or `may_release`
    /// cleared a combat-held player): the entity's end, as chosen by the
    /// policy's [`ExpireTo`]. After this call the core runs the ordinary
    /// despawn path for [`ExpireTo::Despawn`] (`on_leave` remains THE
    /// single despawn funnel — snapshot/membership contracts hang off
    /// it), or keeps everything alive with a `bot_fed` marker for
    /// [`ExpireTo::AiHandover`] (Tur B consumes the marker).
    ///
    /// Default: empty.
    fn on_detach_expired(&mut self, _world: &mut W, _player: PlayerId, _to: ExpireTo) {}

    /// Park-ledger query behind a resume attempt (§5/§7): does the ledger
    /// hold `identity`? See [`ResumeFound`] for the three answers and
    /// their exact client-visible consequences. The `Held` answer is the
    /// parked session's stable [`PlayerId`] — the key the core's own
    /// tables already use.
    ///
    /// Default: [`ResumeFound::Never`] — a logic without parks turns
    /// every identified join into an ordinary fresh join.
    fn resume_lookup(&self, _world: &W, _identity: &str) -> ResumeFound {
        ResumeFound::Never
    }

    /// A resume was accepted: the session moved onto `entity`/`player`
    /// through the fresh transport `conn`. With every table keyed by the
    /// stable [`PlayerId`] (Faz 2), the logic has almost NOTHING to
    /// re-key here any more — its own player-keyed tables kept their keys
    /// across the disconnect. What remains is exactly what is genuinely
    /// session-scoped: consume/update the ledger entry, reset per-session
    /// numbered-input state (DESIGN §14.2: the resumed session numbers
    /// from 1; a logic keeping per-conn sequence state resets it HERE,
    /// under the stable player key), and mark a delta-mode fresh member
    /// so the one-shot full flows to the NEW connection. Full-snapshot
    /// logics need nothing here (every snapshot is full).
    ///
    /// Default: no-op.
    fn on_resume(
        &mut self,
        _world: &mut W,
        _identity: &str,
        _conn: ConnectionId,
        _player: PlayerId,
        _entity: EntityId,
    ) {
    }

    /// Phase 2 — convert buffered actions into component writes.
    fn ingest(&mut self, world: &mut W, ctx: &TickCtx, actions: &mut Vec<Action>);

    /// Phase 3 — run the game systems for this tick.
    fn update(&mut self, world: &mut W, ctx: &TickCtx);

    /// Called when the room shuts down (world is dropped right after).
    fn on_shutdown(&mut self) {}

    /// Handle one correlated request (the RPC pattern; see `crate::rpc`
    /// for the contract: id space, ordering, caps, timeouts).
    ///
    /// The request is guaranteed to belong to a connection that is in
    /// the room (the actor only pulls actions of registered connections)
    /// and to carry a decodable base envelope with `id != 0` (the core
    /// rejects the malformed/uncorrelable cases before this call). The
    /// logic decides the request's fate:
    ///
    /// - [`RequestDecision::Reply`] — answered in this tick;
    /// - [`RequestDecision::Reject`] — a normal rejection, this tick;
    /// - [`RequestDecision::External`] — delegated; the core registers
    ///   the request as pending (subject to the pending caps — an
    ///   over-cap request is answered with a normal rejection even if
    ///   the logic said `External`) and hands the future to a worker;
    /// - `None` — the opcode is not a request this logic handles: the
    ///   core answers with a normal rejection (the client learns "no
    ///   handler" instead of waiting for its own timeout).
    ///
    /// Called once per request, in arrival order, AFTER this tick's
    /// `ingest` (a request sees the world after the tick's fire-and-
    /// forget actions were applied). Synchronous: an `External` decision
    /// must be an OWNING future (`'static`) — the tick body ends long
    /// before the work resolves.
    ///
    /// Default: `None` (a logic without request support gets a
    /// "no handler" answer for every request — no behaviour change for
    /// existing games, whose requests were previously ignored).
    ///
    /// Why this lives on the SHARED supertrait (Faz 3,
    /// `docs/TRAIT-ARCHITECTURE.md` §4): the method only gives a shard
    /// logic the *possibility* of answering requests; making it *work*
    /// needed the actor machinery (pending set, sweep, completion
    /// channel) on [`crate::shard::ShardActor`] — which now runs it with
    /// the same contract as [`RoomActor`]. One method, one decision
    /// vocabulary, two actors.
    fn handle_request(
        &mut self,
        _world: &mut W,
        _ctx: &TickCtx,
        _req: &crate::rpc::RpcRequest,
    ) -> Option<crate::rpc::RequestDecision> {
        None
    }

    /// The match result to report through the actor's result sink when it
    /// shuts down (any shutdown: a control-plane destroy, a server stop).
    /// The actor calls this after [`GameLogic::on_shutdown`], right
    /// before the world is dropped, passing the world (mutably — a
    /// final-state query, e.g. bevy's `Query`, needs it) so the logic can
    /// compute the result from final state without having cached it
    /// (no per-tick cost). `None` = no result (the sink receives
    /// nothing). The payload is game-encoded and opaque to the core.
    ///
    /// Delivery is best-effort (a bounded sink, a synchronous
    /// `try_send`): a full or gone sink drops the result and warns —
    /// a slow result consumer must not stall the teardown.
    ///
    /// Sharded rooms: EVERY shard calls this on its own teardown and
    /// reports through the SAME sink under the same logical room id, so
    /// one logical room yields one payload PER SHARD (the platform's
    /// adapter concatenates/filters; nothing was added to the wire — see
    /// `crate::shard`). Default: no result.
    fn match_result(&mut self, _world: &mut W) -> Option<bytes::Bytes> {
        None
    }

    /// The number of entity records the logic encoded during the most
    /// recent broadcast phase (summed over all groups). The room polls
    /// this exactly **once per step, immediately after the broadcast
    /// phase** (it is the broadcast phase's own metric: the payload is
    /// opaque to the core, so the record count can only come from the
    /// logic that encoded it).
    ///
    /// This is the *overlap* measurement the load test reports: divided by
    /// the broadcastable entity count it says how many times the same
    /// entity was encoded into how many groups' snapshots in one tick
    /// (1.0 for one-group rooms; up to the block overlap for cell AOI; the
    /// visibility-table out-degree for PVS). Default: `0` (untracked) —
    /// the core's own test logics need not implement it.
    fn encoded_records(&mut self) -> u64 {
        0
    }
}

/// Game-side behaviour of a SINGLE-ROOM actor. Faz 3 promoted the two
/// request/result seams ([`GameLogic::handle_request`] /
/// [`GameLogic::match_result`]) onto the SHARED supertrait, so this
/// subtrait no longer adds methods — it remains the compile-time marker
/// that a logic is built for the single-room actor (the same
/// deliberate-subtrait discipline the shard side keeps with its topology
/// hooks; `docs/TRAIT-ARCHITECTURE.md` §3). The RPC/result MACHINERY
/// (pending set, sweep, completion channel, sink) is actor-side state,
/// not trait surface: [`RoomActor`] and [`crate::shard::ShardActor`]
/// each run their own copy of it.
pub trait RoomLogic<W>: GameLogic<W> {}
