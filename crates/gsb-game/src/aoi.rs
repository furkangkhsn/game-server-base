//! [`AoiRoom`]: an Area-of-Interest (AOI) [`RoomLogic`] for the demo game,
//! in its **cell-encoded delta** form.
//!
//! ## Encoding unit vs audience (the design, decided — see the module
//! docs' "Why this shape")
//!
//! The snapshot group key is a **spatial cell** (`GroupKey = Cell`), but
//! the *encoding* no longer happens per group's 3×3 block:
//!
//! - **A cell is the encoding unit.** Each cell encodes its own content
//!   **once per tick** (a *full piece* — all its records — or a *delta
//!   piece* — its exits and changed records against the previous tick).
//!   The pieces are frozen `Bytes`, shared by reference with every group
//!   that needs them (Arc refcount — the same "compute once, share the
//!   bytes" spine, now at cell granularity instead of block granularity).
//! - **A group is the audience.** A group's (cell's) packet is the
//!   *concatenation* of the pieces of the cells in its 3×3 neighborhood
//!   (pre-encoded blocks concatenate into valid protobuf: the repeated
//!   fields simply continue — verified end to end, see `tests/delta_aoi.rs`).
//! - **The full/delta decision is per (group, cell) pair**, and with a
//!   fixed 3×3 neighborhood it reduces to the cell's own transition plus
//!   the group's age:
//!   - a **fresh** group (it had no members last tick) emits a **full**
//!     packet on its first tick (`delta = false`): all its members are new
//!     to the view and have no baseline;
//!   - for an **established** group the packet is a **delta**
//!     (`delta = true`), and per cell: *appeared* (was empty, now has
//!     content) → the cell's **full** records as upserts (the client has
//!     no baseline for that cell); *continues* (content changed) → the
//!     cell's **delta** piece (exits, then updates); *became empty* → a
//!     single **`CellExit` record** (the client forgets every entity it
//!     holds for that cell — one record per cell, whatever the cell's
//!     population); *silent* → nothing.
//!
//! That collapses two historical costs into one change: the encoding is
//! **E records per tick** (each entity is encoded once, in its own
//! cell's piece) instead of ~overlap×E (each entity encoded into every
//! overlapping 3×3 block — the measured overlap was 8.28 for the dense
//! profile), and the delta is not a separate feature: it is what
//! "encode the cell, not the block, against the previous tick" already
//! produces.
//!
//! ## The delta invariant (why the piece can be shared across groups)
//!
//! A cell's delta is `content(now) vs content(previous tick)` — a pure
//! per-cell function, independent of the group. That is only sound if
//! every established group's clients are always in sync with the
//! *previous tick's* content of every cell in their view. The induction:
//! on an emitting tick the group's packet brings every visible cell to
//! the current content (full records, or the exit/update diff against
//! exactly the previous content); on a silent tick the group is silent
//! *because* every visible cell's content equals what its clients hold.
//! Hence, at any tick, an established group's client state for a visible
//! cell equals the previous tick's content of that cell — and the next
//! delta is computed against exactly that. No per-group ledgers are
//! needed at all: the room keeps the current buckets (incrementally
//! maintained — "Dirty cells" below), the per-tick change lists, and,
//! per group, the fact that it was born.
//!
//! The invariant is a server-side one (the per-cell delta is group-
//! independent no matter what the clients hold). A client that is loss-
//! free is exactly in sync with the previous tick's content at every
//! tick; a client that lost frames is not (its state lags). Such a
//! client applies every delta it receives **on top of its stale state,
//! best effort**: the records are absolute (upserts of current
//! positions, forgets by absolute wire id / cell) and idempotent, so
//! misapplication is impossible — the worst case is a stale view (a
//! missed exit may ghost briefly). Exact resync comes from the **full**
//! the server ships on the keep-alive cadence (see
//! [`RoomLogic::keepalive`]) and one-shot via [`RoomLogic::private`] to
//! every fresh group member (see below); recovery is bounded by the
//! keep-alive period. Note a sequence gap is therefore NOT proof of
//! loss — the group's stream is event-driven (a group ships a frame
//! only when its content changes; wire positions are integer-quantized
//! while motion is fractional, so even a moving entity is silent for a
//! run of ticks) — the full is the guarantee, not the gap.
//!
//! ## Dirty cells (per-tick work ∝ movers, not entities)
//!
//! The delta is **not** computed by diffing this tick's buckets against
//! last tick's: the room keeps only the current buckets (maintained
//! **incrementally**) plus, per tick, each *touched* cell's **change
//! list** — the exits and updates that happened to it. The change list
//! *is* the diff, so a cell's delta piece is assembled from it directly
//! and no per-cell content comparison is ever run (diffing was
//! O(occupied cells × cell size) per tick; assembling is O(changed
//! records)).
//!
//! **The dirty marking is structural, not a discipline.** A cell is
//! dirty exactly when some entity's `Position` was written this tick —
//! and that fact is recorded by **bevy's own change detection**
//! (`Changed<Position>`), not by the room calling a `bump()` method:
//! the mark is set inside bevy's write path, so *any* writer — the
//! movement system, `ingest`, a direct `world.entity_mut` in a test,
//! future game code — marks the cell dirty by construction. This is the
//! same shape as the `WireId` fix (private field, single mint point):
//! the previous design's comment-only "writers must remember to
//! invalidate" discipline was bitten three times in this project
//! (see `docs/ROADMAP.md`), so the mark now lives where it cannot be
//! forgotten. `DESIGN.md` §7 avoided bevy's *observer/event* change API
//! for the room's input path; standalone `Changed<T>` with a manual
//! `World::clear_trackers()` at the end of `update` is the same
//! mechanism used through its query interface — the baseline is the end
//! of the previous `update`, so the query window covers the systems'
//! writes, between-update spawns (joins), and direct writes alike
//! (verified by `tests/zzz_probe_bevey.rs` during design; probe deleted,
//! behaviour locked by the change-detection tests in this module).
//!
//! What the query window *does not* cover — and how each case is
//! handled:
//!
//! - **Despawns are not writes.** `on_leave` despawns the entity in the
//!   CONTROL phase, so the query (which iterates live entities) cannot
//!   see it: the leave parks `(entity, wire id, cell)` in
//!   `pending_removals`, which `update` applies against the buckets
//!   (the cell the entity was in is read from `last_cell`, written in
//!   `update` and nowhere else). A join+leave within one tick parks
//!   nothing — the entity never made it into `last_cell`, hence never
//!   into the buckets.
//! - **Quantization.** Wire positions are i32 truncations of f32 motion:
//!   a sub-cell move can leave the wire position untouched. The dirty
//!   query still flags the write, and the same-cell branch compares the
//!   OLD wire record against the new one — an unchanged wire position
//!   records nothing (the cell can still classify `Silent`), so the
//!   stream is content-identical to the diff-based design.
//! - **Same-value rewrites** are tracked by bevy (every write dirties,
//!   even an identical one) and degrade to the no-op above.
//!
//! **Occupancy and birth, order-independently.** A cell's appeared/exited
//! flag is a pure function of (occupied at the end of the last `update`,
//! occupied now): `update` keeps `prev_occupied` **frozen** while the
//! dirty loop runs (it is rolled only afterwards, per touched cell, from
//! the final bucket state), so same-tick exit+entry into the same cell
//! cannot flip either flag. Group birth (a member count going 0 → >0)
//! is reconstructed from the net member events —
//! `before = now − in + out`, evaluated per touched cell after all
//! events — so a same-tick member exit and a different member's entry
//! into the same cell cannot fake a birth. The per-tick passes (dirty
//! loop, flag/birth roll) iterate the touched cells / change lists,
//! i.e. O(movers) — never the occupied cells or the entity count.
//! (An earlier literal reading of the birth rule as "cells with members
//! now minus cells with members last tick" would miss a member joining a
//! cell that already held non-member content — the cell was in both
//! sets; the member-count formulation is the correct generalization, and
//! in the member-only case it coincides with the literal reading. The
//! missed-birth consequence is observational only: the new member is
//! baselined by its one-shot private full in the same batch, see
//! "Late joiners".)
//!
//! ## Ordering inside a delta packet
//!
//! The layout is fixed and part of the protocol (`game.proto`):
//! `[header: sequence, delta][removed: entity exits][cell_exits][entities:
//! updates]`. An entity moving from cell X to cell Y is *exited* from X
//! (X's delta piece reports its own lost membership — cell-local and
//! cheap) and *updated* in Y; a client that sees both cells must process
//! the exit before the update (it does: exits come first), a client that
//! sees only X sees the exit (the entity correctly vanishes from its
//! view), one that sees only Y sees the update (it correctly appears).
//!
//! ## Late joiners and group crossings (the one-shot private full)
//!
//! A connection that (re)joins a group, or crosses into a new cell's
//! group, has **no baseline** for that group's view — a delta has nothing
//! to apply against ("what changed" is meaningless without "what is
//! there"). It therefore receives a one-shot **full** of its new group's
//! 3×3 via the per-connection `private` frame (once per join/crossing),
//! and the group stays in delta mode for everyone (no per-connection
//! groups). If the group's own emission this tick is already a full
//! (a fresh group's first packet, or a keep-alive full), that frame is
//! in the same batch ahead of the private frame and the one-shot is
//! skipped. This is the AOI form of the `late_joiner_receives_full_world_
//! snapshot` guarantee.
//!
//! ## Keep-alive (the delta's recovery path)
//!
//! Re-sending the group's last payload on a keep-alive tick is
//! meaningless in delta mode: the last payload is a *delta* — a client
//! that missed it has no baseline to apply it to, and a current client
//! would double-apply it. The keep-alive tick therefore ships a **fresh
//! full** for the group (encoded from the cells' full pieces — shared
//! across groups, cached per tick), *whether the group is active or
//! silent*: on an active group's tick the full replaces that tick's
//! delta (a superset — healthy clients simply take the full), and a
//! client that lost one or more deltas is healed within one keep-alive
//! **period** (≤ 1 s + 1 tick at the defaults) regardless of how busy
//! its group is. For a silent group the full's content equals what
//! healthy clients already hold (the delta invariant), so it is a no-op
//! for them. The cost is one full *encode* per group per keep-alive tick
//! (≈ the bytes the old keep-alive re-sent, once per second).
//!
//! ## Security parameter: the visible cell set
//!
//! The group's visible cell set is its **3×3 neighborhood, content-
//! agnostic and omnidirectional**: a cell contributes to the group's
//! packet iff it is in the neighborhood; empty cells contribute nothing
//! (nothing leaks from an empty cell). The visibility boundary is
//! quantized to cells: an entity is broadcast iff its cell is in the
//! group's 3×3, i.e. the error band is at most one cell edge beyond the
//! group cell's boundary (≤ `cell_size` world units; worst case ~2×
//! `cell_size` from a member at the group cell's far edge). The
//! omnidirectional block is the price of the shared-bytes model: a finer
//! "lit cells" notion (per-player facing/vision) would make the snapshot
//! a function of the *player*, not the *cell*, which the group-snapshot
//! model cannot express without per-connection payloads — that general-
//! ization is a ROADMAP item, not part of this round. `cell_size` is
//! the single knob that sets both the concealment resolution and the
//! leak band (see `docs/DESIGN.md` §8).
//!
//! **Why a block keyed on the cell, and not a per-player radius?** A
//! per-player radius would make the snapshot a function of the *player*,
//! not the *cell* — every player in a cell would need a different
//! payload, which the group-snapshot model (one `Bytes` per group,
//! shared by reference) cannot express without per-connection groups or
//! a core change. A block keyed on the cell keeps the "compute once per
//! cell, share the `Bytes`" property exactly. (Alternatives rejected in
//! the original AOI turn stand: own-cell-only — the boundary blind spot;
//! larger blocks — ~2.8× the encoding cost and bigger payloads.)
//!
//! ## Cell size
//!
//! `cell_size` (world units per cell edge) is the one tunable: it must
//! keep a cell's 3×3 block under `max_snapshot_bytes` at the expected
//! *peak* entity density, and it sets the leak band (above).
//!
//! ## Cells are computed from the WIRE position
//!
//! The cell of an entity is `floor(wire_x / cell_size)` on the
//! **integer** (wire) coordinates, not the f32 simulation position: the
//! client holds only wire coordinates and must compute the *same* cell
//! to service `CellExit` records (forget every entity it holds for the
//! exited cell). Server and client therefore bucket by the truncated
//! position; the group key (`group_of`) uses the same formula, so a
//! connection's group is exactly the cell its entity's records land in.
//!
//! ## Invariants preserved (see `tests/aoi.rs`, `tests/delta_aoi.rs`)
//!
//! - **Identity**: the wire id is minted once and never changes; a
//!   cell-changing entity keeps it.
//! - **Late join / group crossing**: a fresh group member sees its full
//!   visibility block (one-shot private full, or the group's own fresh
//!   full in the same batch).
//! - **Broadcast set**: exactly "has a `Position`" (orphan stamping in
//!   `update`, structural like `DemoRoom`).
//! - **Convergence**: the delta stream and the full stream produce the
//!   same client view of the same world (two paths, one result).
//! - **No ghosts, no duplicates**: an entity in a cell the client cannot
//!   see is neither drawn nor duplicated; exits (per entity, per cell)
//!   are delivered on the group's packet.

use std::collections::{HashMap, HashSet};
use std::hash::Hash;

use bevy_ecs::prelude::{Changed, Entity, World};
use bytes::{BufMut, Bytes, BytesMut};
use gsb_core::id::{ConnectionId, EntityId};
use gsb_core::room::{Action, RoomLogic, TickCtx};
use gsb_ecs::SystemRunner;
use prost::encoding::varint::encode_varint;
use prost::Message;

use crate::components::{Position, WireId};
use crate::op;



/// A spatial cell of the world grid — the AOI group key. Cell indices
/// are the floor of (wire position / `cell_size`) — see the module
/// docs, "Cells are computed from the WIRE position".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Cell(pub i32, pub i32);

/// How many cells the visibility block extends in each direction from
/// the player's cell. `1` ⇒ a 3×3 block (the cell + its 8 ring-1
/// neighbors).
const RADIUS: i32 = 1;

/// The (dx, dy) offsets of the visibility block centered on a cell
/// (deterministic order: the assembly order of the group's packet).
const BLOCK_OFFSETS: [(i32, i32); 9] = [
    (-RADIUS, -RADIUS), (0, -RADIUS), (RADIUS, -RADIUS),
    (-RADIUS, 0), (0, 0), (RADIUS, 0),
    (-RADIUS, RADIUS), (0, RADIUS), (RADIUS, RADIUS),
];

/// The cell containing the WIRE (integer) position (see the module
/// docs): `floor(x / cell_size)` on the integer coordinates, so the
/// client — which holds only wire coordinates — computes the same cell.
#[inline]
fn cell_of(x: i32, y: i32, cell_size: f32) -> Cell {
    Cell(
        (x as f32 / cell_size).floor() as i32,
        (y as f32 / cell_size).floor() as i32,
    )
}

/// The per-tick classification of one cell (a pure function of the
/// cell's change list and its occupancy baseline — identical for every
/// group that sees the cell; see the module docs, "The delta invariant"
/// and "Dirty cells").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CellFrag {
    /// Empty now, empty before, and no change recorded this tick:
    /// nothing for any group.
    Silent,
    /// Non-empty before, empty now: the group's packet carries one
    /// `CellExit` record for it (the client forgets the whole cell in
    /// one record).
    Exited,
    /// Empty before, non-empty now: no baseline exists for the cell —
    /// its FULL records go into the group's packet (upserts in a delta
    /// packet; full content in a fresh group's packet).
    Appeared,
    /// Non-empty before and now, content changed: the cell's delta
    /// piece (its change list: exits + updates).
    Delta,
}

/// One cell's changes this tick — the delta's source of truth (module
/// docs, "Dirty cells"): the change list *is* the diff, so no per-cell
/// content comparison is ever run. Built incrementally in `update` from
/// the dirty set; persistent map, cleared in place each tick.
#[derive(Default)]
struct CellChanges {
    /// Records whose wire content changed, or that newly occupy the cell
    /// (wire id, wire x, wire y). Encoded as `entities` upserts (field
    /// 2) — except for an appeared cell, whose group gets the cell's
    /// FULL piece instead (the upserts would be redundant: the client
    /// has no baseline for a cell that was empty).
    updates: Vec<(u64, i32, i32)>,
    /// Wire ids that left the cell (a cell-to-cell move or a despawn).
    /// Encoded as `removed` (field 3) — except when the whole cell
    /// exited, in which case one `CellExit` record supersedes them.
    exits: Vec<u64>,
    /// The cell was empty at the end of the last tick (set at the end
    /// of `update`, order-independently — see there).
    appeared: bool,
    /// The cell is empty now (and was not empty then) — same evaluation.
    exited: bool,
}

/// The per-tick member-event counters of one touched cell (module docs,
/// "Dirty cells"): the order-independent group-birth arithmetic
/// reconstructs the before-tick member count from
/// `now − in + out`, so a same-tick exit+entry into the same cell
/// cannot fake a birth.
#[derive(Default)]
struct TouchInfo {
    /// Member entities that entered this cell this tick (joins into it,
    /// cell crossings into it).
    member_in: u32,
    /// Member entities that left it (crossings out, leavers).
    member_out: u32,
}

/// The AOI room: spatial group key (audience), per-cell encoding (unit),
/// per-cell delta against the previous tick, one-shot private fulls for
/// fresh group members, keep-alive fulls as the loss-recovery path.
pub struct AoiRoom {
    runner: SystemRunner,
    /// Which entity belongs to which connection.
    conn_entity: HashMap<ConnectionId, Entity>,
    /// The room's single wire-identity counter (see module docs,
    /// "Invariants preserved" / `game.proto`).
    next_wire_id: u64,
    /// World units per cell edge (see module docs, "Cell size" — it also
    /// sets the leak band, "Security parameter").
    cell_size: f32,
    /// Half-size of the square spawn map (see `gsb_game::room::spawn_pos`);
    /// configuration, not a strategy decision.
    spawn_half: f32,
    /// Per-connection input sequence state (strategy-independent; see
    /// `crate::common::ingest` / `emit_ack`).
    input: HashMap<ConnectionId, crate::common::InputState>,
    /// Per-connection view baseline: `conn → the cell whose FULL view was
    /// last delivered to it` (via the one-shot private full, or via the
    /// group's own full in the same batch). A connection whose entry is
    /// missing or names another cell has no baseline for its current
    /// group's view and gets a one-shot private full (see `private`).
    conn_view: HashMap<ConnectionId, Cell>,
    /// The current buckets: `cell → (wire id → (x, y))` — the content of
    /// every cell, maintained **incrementally** in `update` from the
    /// dirty set (module docs, "Dirty cells"): an entity enters/leaves a
    /// bucket when its wire position/cell changes. Invariant: after
    /// `update`, the buckets equal the world's current content (they are
    /// temporarily stale during the CONTROL phase — joins not yet
    /// bucketed, leavers not yet removed — and nothing reads them
    /// between `update` and the next `update`'s dirty query).
    buckets: HashMap<Cell, HashMap<u64, (i32, i32)>>,
    /// The cells that were occupied (any content, members or not) at the
    /// end of the last `update`: the appearance/exit baseline. Rolled
    /// once per touched cell at the end of `update` (the per-tick cost
    /// is proportional to the movers, not to the occupied cells).
    prev_occupied: HashSet<Cell>,
    /// Each bucketed entity's cell at the end of the last `update`
    /// (written in `update`, read by the dirty query, `on_leave`'s
    /// removal parking, and `group_of` — the per-connection group
    /// lookups of the broadcast phase are O(1) table reads, not
    /// per-connection world queries).
    last_cell: HashMap<Entity, Cell>,
    /// The member count of each cell (maintained incrementally in
    /// `update`; empty entries removed): the group-birth arithmetic's
    /// "now" input (module docs, "The full/delta decision" + "Dirty
    /// cells").
    member_counts: HashMap<Cell, u32>,
    /// The cells touched this tick (a cell is touched when one of its
    /// records is added to or removed from its bucket) with their
    /// member-event counters (persistent map, cleared in place each
    /// tick): the per-cell flag/birth pass at the end of `update`
    /// iterates exactly this map — O(movers), never O(cells).
    touched: HashMap<Cell, TouchInfo>,
    /// Each touched cell's change list for this tick (persistent map,
    /// cleared in place each tick): the delta's source of truth — the
    /// change list *is* the diff (no per-cell content comparison is
    /// ever run; module docs, "Dirty cells").
    cell_changes: HashMap<Cell, CellChanges>,
    /// Leavers parked in `on_leave` — `(entity, wire id, cell)` — and
    /// applied in `update`: a despawn is not a component write, so it is
    /// invisible to the `Changed<Position>` query (module docs, "Dirty
    /// cells").
    pending_removals: Vec<(Entity, u64, Cell)>,
    /// The member entities (maintained in `on_join`/`on_leave`): the
    /// dirty loop's O(1) membership test (member counts, birth
    /// arithmetic).
    members: HashSet<Entity>,
    /// Cells with members now but none last tick: their groups are fresh
    /// this tick and must emit a FULL packet on their first tick (module
    /// docs, "The full/delta decision").
    born_groups: HashSet<Cell>,
    /// The global tick of the current step (set in `update`): the
    /// `private` seam has no `TickCtx`, so the tick it stamps into
    /// payloads comes from here.
    tick: u64,
    // ── Per-tick caches (cleared in `update`; computed lazily in the
    //    broadcast phase — the room calls `snapshot`/`keepalive`/
    //    `private` once per group/conn in unspecified order, and the
    //    cache makes the pieces order-independent: the same (cell, kind)
    //    is computed once, shared as frozen `Bytes` by reference). ──
    /// Each cell's encoded FULL records (the `entities` entries, field 2)
    /// of its current content; shared by every consumer this tick (a
    /// fresh group's full, a keep-alive full, a one-shot private full,
    /// an *appeared* cell's upserts).
    full_pieces: HashMap<Cell, Bytes>,
    /// Each changed cell's encoded delta: the `removed` entries
    /// (field 3; `None` when the cell lost no entity) + the `entities`
    /// entries (field 2) of its changed/new records (assembled from the
    /// cell's change list — see [`Self::cell_changes`]).
    delta_pieces: HashMap<Cell, (Option<Bytes>, Bytes)>,
    /// Each exited cell's encoded `cell_exits` entry (field 4).
    exit_markers: HashMap<Cell, Bytes>,
    /// The assembled FULL snapshot of a cell's 3×3 view (header + the
    /// full pieces), shared between the fresh-group packet, the keep-
    /// alive full, and the one-shot private full.
    full_view: HashMap<Cell, Bytes>,
    /// The scratch behind [`Self::full_view`] (reused across assemblies;
    /// `split_to` hands out zero-copy views — no per-assembly allocation).
    full_scratch: BytesMut,
    /// The per-tick, per-cell classification cache: `classify` is a pure
    /// function of the per-tick change list, which does not change during
    /// the tick, so the first group that asks a cell computes its
    /// fragment **once** and every later group (every later *pass* of the
    /// same group — the 3×3 block is walked four times per group) reuses
    /// it — including the negative answer (`Silent`), which is a hash
    /// miss on the change list, not a scan.
    frag_cache: HashMap<Cell, CellFrag>,
    /// The groups that emitted a FULL this tick (a fresh group in
    /// `snapshot`, a silent group in `keepalive`): a member of such a
    /// group is baselined by that frame (it precedes the private frame in
    /// the batch), so `private` skips its one-shot full.
    group_full_emitted: HashSet<Cell>,
    /// Entity records encoded into pieces so far this tick (polled once
    /// per step by the room via `RoomLogic::encoded_records`) — the
    /// overlap measurement: ~E per tick in steady state (one encoding per
    /// entity, in its own cell's piece).
    encoded: u64,
}

impl AoiRoom {
    /// Build an AOI room with the given `cell_size` (world units per cell
    /// edge) over the default 100×100 spawn arena. Clamped to a sane
    /// minimum so a degenerate `0` cannot produce a single infinite cell.
    #[must_use]
    pub fn new(cell_size: f32) -> Self {
        Self::with_spawn_half(cell_size, crate::room::DEFAULT_SPAWN_HALF)
    }

    /// Build an AOI room over a square spawn map of half-size `half` (see
    /// `gsb_game::room::DemoRoom::with_spawn_half`).
    #[must_use]
    pub fn with_spawn_half(cell_size: f32, half: f32) -> Self {
        Self {
            runner: crate::common::movement_runner(),
            conn_entity: HashMap::new(),
            next_wire_id: 0,
            cell_size: cell_size.max(0.5),
            spawn_half: half.max(1.0),
            input: HashMap::new(),
            conn_view: HashMap::new(),
            buckets: HashMap::new(),
            prev_occupied: HashSet::new(),
            last_cell: HashMap::new(),
            member_counts: HashMap::new(),
            touched: HashMap::new(),
            cell_changes: HashMap::new(),
            pending_removals: Vec::new(),
            members: HashSet::new(),
            born_groups: HashSet::new(),
            tick: 0,
            full_pieces: HashMap::new(),
            delta_pieces: HashMap::new(),
            exit_markers: HashMap::new(),
            full_view: HashMap::new(),
            full_scratch: BytesMut::new(),
            frag_cache: HashMap::new(),
            group_full_emitted: HashSet::new(),
            encoded: 0,
        }
    }

    /// The snapshot header: `sequence` (field 1, varint) + the `delta`
    /// flag (field 5; written only when true — a `false`/absent flag
    /// means FULL, per `game.proto`).
    fn write_header(buf: &mut BytesMut, tick: u64, delta: bool) {
        buf.put_u8(0x08); // field 1 (sequence), varint
        encode_varint(tick, buf);
        if delta {
            buf.put_u8(0x28); // field 5 (delta), varint
            buf.put_u8(1);
        }
    }

    /// Encode `records` as `entities` entries (field 2, length-
    /// delimited) — one pre-encoded piece, shareable by reference.
    /// (Encoding straight into the buffer: no per-record allocation.)
    fn encode_records(records: &[(u64, i32, i32)]) -> Bytes {
        let mut out = BytesMut::new();
        for &(wire, x, y) in records {
            let rec = crate::game::EntityRecord {
                entity: wire,
                x,
                y,
            };
            out.put_u8(0x12); // field 2 (entities), length-delimited
            encode_varint(rec.encoded_len() as u64, &mut out);
            rec.encode(&mut out)
                .expect("protobuf encode into an in-memory buffer failed");
        }
        out.freeze()
    }

    /// Encode `exits` as `removed` entries (field 3, varint) — the
    /// entity-exit piece of a delta.
    fn encode_exits(exits: &[u64]) -> Bytes {
        let mut out = BytesMut::new();
        for &wire in exits {
            out.put_u8(0x18); // field 3 (removed), varint
            encode_varint(wire, &mut out);
        }
        out.freeze()
    }

    /// Encode one `cell_exits` entry (field 4, length-delimited) for
    /// `cell` — the single record that makes the client forget a whole
    /// cell.
    fn encode_cell_exit(cell: Cell) -> Bytes {
        let msg = crate::game::CellExit {
            x: cell.0,
            y: cell.1,
        };
        let mut out = BytesMut::new();
        out.put_u8(0x22); // field 4 (cell_exits), length-delimited
        encode_varint(msg.encoded_len() as u64, &mut out);
        msg.encode(&mut out)
            .expect("protobuf encode into an in-memory buffer failed");
        out.freeze()
    }

    /// The cell's FULL piece (its complete current content, encoded once
    /// per tick; `None` for an empty cell).
    fn full_piece(&mut self, c: &Cell) -> Option<Bytes> {
        if !self.full_pieces.contains_key(c)
            && let Some(bucket) = self.buckets.get(c)
        {
            let records: Vec<(u64, i32, i32)> =
                bucket.iter().map(|(&w, &(x, y))| (w, x, y)).collect();
            self.encoded += records.len() as u64;
            self.full_pieces.insert(*c, Self::encode_records(&records));
        }
        self.full_pieces.get(c).cloned()
    }

    /// A changed cell's delta piece: `(exits, updates)` assembled from
    /// the cell's change list — the change list *is* the diff (module
    /// docs, "Dirty cells"), so no per-cell content comparison is run
    /// (encoded once per tick). `None` when the cell has no change list
    /// (the caller classifies first; such a cell is silent for the
    /// tick).
    fn delta_piece(&mut self, c: &Cell) -> Option<&(Option<Bytes>, Bytes)> {
        if !self.delta_pieces.contains_key(c)
            && let Some(ch) = self.cell_changes.get(c)
            && (!ch.exits.is_empty() || !ch.updates.is_empty())
        {
            self.encoded += ch.updates.len() as u64;
            self.delta_pieces.insert(
                *c,
                (
                    (!ch.exits.is_empty()).then(|| Self::encode_exits(&ch.exits)),
                    Self::encode_records(&ch.updates),
                ),
            );
        }
        self.delta_pieces.get(c)
    }

    /// One `CellExit` marker for an exited cell (encoded once per tick).
    fn exit_marker(&mut self, c: &Cell) -> Bytes {
        if !self.exit_markers.contains_key(c) {
            self.exit_markers.insert(*c, Self::encode_cell_exit(*c));
        }
        self.exit_markers.get(c).expect("inserted above").clone()
    }

    /// The per-tick classification of `c` (see [`CellFrag`]).
    ///
    /// Computed **once per cell per tick, including the negative answer**
    /// (the memoization of this round's item (a)): the per-tick change
    /// list does not change during the tick, so the first caller's
    /// result is valid for every later caller (every other group, and
    /// every other *pass* of the same group's `snapshot` — the 3×3 block
    /// is walked four times per group). The `Silent` answer is cached
    /// too, and with the dirty set (item (b)) it costs one hash miss on
    /// the change list — a cell that had no change this tick is never
    /// scanned, and a cell that did is classified by its flags, not by a
    /// comparison. Within a tick the classification is a pure function
    /// of the (change list, occupancy baseline) pair, both frozen until
    /// the next `update` — memoization is exact, not an approximation.
    fn classify(&mut self, c: &Cell) -> CellFrag {
        if let Some(&frag) = self.frag_cache.get(c) {
            return frag;
        }
        let frag = match self.cell_changes.get(c) {
            // No change recorded this tick: nothing for any group
            // (covers both "empty all along" and "occupied, untouched" —
            // the latter is impossible to have *content* in without a
            // change record, so both read the same way).
            None => CellFrag::Silent,
            // The flags are set at the end of `update`, order-
            // independently (see there); `appeared` outranks `exited`
            // (they are mutually exclusive by construction).
            Some(ch) if ch.appeared => CellFrag::Appeared,
            Some(ch) if ch.exited => CellFrag::Exited,
            Some(_) => CellFrag::Delta,
        };
        self.frag_cache.insert(*c, frag);
        frag
    }

    /// The assembled FULL snapshot of `cell`'s 3×3 view (header with
    /// `delta = false` + the full pieces of every non-empty cell) —
    /// computed once per tick and shared (a fresh group's packet, the
    /// keep-alive full, and the one-shot private full all reuse these
    /// bytes).
    fn full_of(&mut self, cell: &Cell) -> Bytes {
        if let Some(bytes) = self.full_view.get(cell) {
            return bytes.clone();
        }
        self.full_scratch.clear();
        Self::write_header(&mut self.full_scratch, self.tick, false);
        for (dx, dy) in BLOCK_OFFSETS {
            let c = Cell(cell.0 + dx, cell.1 + dy);
            if let Some(piece) = self.full_piece(&c) {
                self.full_scratch.extend_from_slice(&piece);
            }
        }
        let bytes = self.full_scratch.split_to(self.full_scratch.len()).freeze();
        self.full_view.insert(*cell, bytes.clone());
        bytes
    }

    /// Mark `c` as touched this tick (the end-of-`update` flag/birth
    /// pass iterates exactly the touched cells — proportional to the
    /// movers, not to the world).
    #[inline]
    fn touch_cell(&mut self, c: Cell) {
        self.touched.entry(c).or_default();
    }

    /// Count a member entering (`in = true`) or leaving (`in = false`)
    /// cell `c` — the order-independent birth arithmetic's input (the
    /// call also marks the cell touched).
    #[inline]
    fn member_event(&mut self, c: Cell, in_: bool) {
        let t = self.touched.entry(c).or_default();
        if in_ {
            t.member_in += 1;
        } else {
            t.member_out += 1;
        }
    }
}

impl RoomLogic<World> for AoiRoom {
    type GroupKey = Cell;

    fn snapshot_op(&self) -> u16 {
        op::WORLD_SNAPSHOT
    }
    fn private_op(&self) -> u16 {
        op::PRIVATE
    }

    /// The connection's group is the cell its entity's records land in
    /// (re-evaluated every tick by the room — a crossing player changes
    /// cell and thus group, and starts receiving the new cell's packets;
    /// it receives a one-shot private full of the new view, see
    /// `private`). Read from the `last_cell` table (item (b): 4a calls
    /// this once per connection per tick — a world query per connection
    /// is O(N) ECS work per tick with no structural way to shrink it;
    /// the table makes it O(1) and it stays current because `update`
    /// always precedes the broadcast phase). The world fallback covers
    /// the one window where the table has no entry (an entity spawned
    /// without a `Position`, which the dirty query cannot see until it
    /// is stamped — defensive; `on_join` always spawns with one).
    fn group_of(&self, world: &World, conn: ConnectionId) -> Cell {
        let Some(&entity) = self.conn_entity.get(&conn) else {
            return Cell(0, 0);
        };
        if let Some(&c) = self.last_cell.get(&entity) {
            return c;
        }
        let pos = world.entity(entity).get::<Position>().copied().unwrap_or_default();
        cell_of(pos.x as i32, pos.y as i32, self.cell_size)
    }

    /// Assemble `cell`'s packet from this tick's pieces (module docs):
    /// a fresh group gets a FULL packet; an established group gets a
    /// DELTA packet (exits, then cell exits, then updates) — or nothing
    /// (returns `false`) when the whole 3×3 is silent for it.
    fn snapshot(
        &mut self,
        _world: &mut World,
        ctx: &TickCtx,
        cell: &Cell,
        out: &mut bytes::BytesMut,
    ) -> bool {
        debug_assert_eq!(ctx.tick, self.tick, "update must precede snapshot");
        if self.born_groups.contains(cell) {
            // Fresh group: every member is new to this view — the first
            // packet is a full (delta=false), so the members end the tick
            // baselined (the invariant starts from here).
            self.group_full_emitted.insert(*cell);
            let full = self.full_of(cell);
            out.extend_from_slice(&full);
            return true;
        }
        // Established group: a delta packet (delta=true) — but only when
        // at least one cell has something to say (silence writes no
        // bytes; the core's scratch stays empty on a `false`).
        let mut any = false;
        for (dx, dy) in BLOCK_OFFSETS {
            let c = Cell(cell.0 + dx, cell.1 + dy);
            match self.classify(&c) {
                CellFrag::Silent => {}
                _ => any = true,
            }
        }
        if !any {
            return false;
        }
        Self::write_header(out, ctx.tick, true);
        // Pass 1: entity exits (field 3) of every cell — exits before
        // updates, so a cell-to-cell move is exited from its source
        // before it is updated in its target.
        for (dx, dy) in BLOCK_OFFSETS {
            let c = Cell(cell.0 + dx, cell.1 + dy);
            if matches!(self.classify(&c), CellFrag::Delta)
                && let Some((exits, _)) = self.delta_piece(&c)
                && let Some(e) = exits
            {
                out.extend_from_slice(e);
            }
        }
        // Pass 2: cell exits (field 4) — one record per cell that became
        // empty (the client forgets the whole cell at once).
        for (dx, dy) in BLOCK_OFFSETS {
            let c = Cell(cell.0 + dx, cell.1 + dy);
            if matches!(self.classify(&c), CellFrag::Exited) {
                out.extend_from_slice(&self.exit_marker(&c));
            }
        }
        // Pass 3: updates (field 2): delta pieces' changed records, and
        // appeared cells' full records (upserts — the client has no
        // baseline for a cell that was empty).
        for (dx, dy) in BLOCK_OFFSETS {
            let c = Cell(cell.0 + dx, cell.1 + dy);
            match self.classify(&c) {
                CellFrag::Delta => {
                    if let Some((_, updates)) = self.delta_piece(&c) {
                        out.extend_from_slice(updates);
                    }
                }
                CellFrag::Appeared => {
                    if let Some(piece) = self.full_piece(&c) {
                        out.extend_from_slice(&piece);
                    }
                }
                _ => {}
            }
        }
        true
    }

    /// Keep-alive (on the cadence tick, whether this group emitted this
    /// tick or not): a freshly encoded FULL snapshot of the group's view
    /// (module docs, "Keep-alive"). The cached last payload is a delta —
    /// re-sending it would be meaningless (a client that missed it has no
    /// baseline; a current client would double-apply); the fresh full
    /// heals any client that lost one or more deltas, bounding the
    /// recovery to the keep-alive period whether the group is active or
    /// silent.
    fn keepalive(
        &mut self,
        _world: &mut World,
        _ctx: &TickCtx,
        group: &Cell,
        _last: Option<&bytes::Bytes>,
        out: &mut bytes::BytesMut,
    ) -> bool {
        debug_assert_eq!(_ctx.tick, self.tick, "update must precede keepalive");
        self.group_full_emitted.insert(*group);
        let full = self.full_of(group);
        out.extend_from_slice(&full);
        true
    }

    fn encoded_records(&mut self) -> u64 {
        let n = self.encoded;
        self.encoded = 0;
        n
    }

    /// The per-connection private frame: a one-shot FULL view for a
    /// connection that (re)joined a group or crossed into a new cell's
    /// group (it has no baseline for the new view — a delta has nothing
    /// to apply against; module docs, "Late joiners and group
    /// crossings"), unless the group's own emission this tick was already
    /// a full (fresh group / keep-alive — that frame precedes the private
    /// frame in the same batch and baselines the connection). Otherwise:
    /// the pending input acknowledgment (Section A; a few bytes per
    /// advanced tick, zero otherwise).
    fn private(
        &mut self,
        _world: &mut World,
        conn: ConnectionId,
        group: &Cell,
        out: &mut bytes::BytesMut,
    ) -> bool {
        // The room passes the connection's current group (re-evaluated
        // every tick in phase 4a) — the previous re-derivation
        // (`conn_cell`: two table lookups per connection per tick) was a
        // measured slice of the idle floor.
        let c = *group;
        let baselined = self.conn_view.get(&conn).copied() == Some(c);
        if !baselined {
            if self.group_full_emitted.contains(&c) {
                // The group's own full is in this batch (ahead of
                // this frame): the baseline is established there.
                self.conn_view.insert(conn, c);
            } else {
                // The one-shot private full (one per join/crossing):
                // the frame is the `Private` message (opcode 1004) —
                // the pre-encoded WorldSnapshot bytes ride in the
                // `snapshot` oneof (field 2, length-delimited).
                let full = self.full_of(&c);
                out.put_u8(0x12); // Private field 2 (snapshot), LEN
                encode_varint(full.len() as u64, out);
                out.extend_from_slice(&full);
                self.conn_view.insert(conn, c);
                return true;
            }
        }
        crate::common::emit_ack(&mut self.input, conn, out)
    }

    fn on_join(&mut self, world: &mut World, conn: ConnectionId) -> EntityId {
        // Shared spawn path (input session reset included): deterministic
        // spawn point, fresh wire identity, connection→entity table.
        // `conn_view` deliberately gets NO entry here: the first
        // `private` call (this tick's fan-out, or the next tick's)
        // delivers the one-shot full and records the baseline — via the
        // group's own fresh full when the group is born, or via the
        // private frame otherwise.
        let wire = crate::common::on_join(
            &mut self.conn_entity,
            &mut self.next_wire_id,
            self.spawn_half,
            world,
            conn,
            &mut self.input,
        );
        // Maintain the member-entity set (the dirty loop's O(1)
        // membership test — it selects which of the changed entities
        // count toward the per-cell member arithmetic).
        if let Some(&entity) = self.conn_entity.get(&conn) {
            self.members.insert(entity);
        }
        wire
    }

    fn on_leave(&mut self, world: &mut World, conn: ConnectionId) {
        // The core guards stale leaves before calling this; a genuine
        // leave drops the entity, the input session, and the view
        // baseline (a re-join is a new session: fresh input state, fresh
        // one-shot full).
        if let Some(&entity) = self.conn_entity.get(&conn) {
            self.members.remove(&entity);
            // Despawns are NOT component writes: the `Changed<Position>`
            // query in `update` cannot see the entity once it is gone,
            // so the removal must be parked here (module docs, "Dirty
            // cells") — the wire id and the cell the entity occupied at
            // the end of the last `update` (`last_cell`, written in
            // `update` and nowhere else). A join+leave inside one tick
            // parks nothing: the entity never made it into `last_cell`
            // (no `update` ran between the join and the leave), so it
            // never entered the buckets — nothing to remove, no member
            // count to undo.
            let wire = world
                .entity(entity)
                .get::<WireId>()
                .map(|w| w.get());
            let cell = self.last_cell.get(&entity).copied();
            if let (Some(wire), Some(cell)) = (wire, cell) {
                self.pending_removals.push((entity, wire, cell));
            }
        }
        crate::common::on_leave(&mut self.conn_entity, world, conn, &mut self.input);
        self.conn_view.remove(&conn);
    }

    fn ingest(&mut self, world: &mut World, _ctx: &TickCtx, actions: &mut Vec<Action>) {
        crate::common::ingest(&self.conn_entity, world, actions, &mut self.input)
    }

    fn update(&mut self, world: &mut World, ctx: &TickCtx) {
        crate::common::run_systems(&mut self.runner, world, ctx);
        // Orphan stamping (idempotent, mirrors `DemoRoom`): entities with
        // a `Position` but no `WireId` get the next serial, so the
        // broadcast set is exactly "has a `Position`". It runs BEFORE the
        // dirty query below: the query requires a `WireId`, and a
        // stamped orphan's `Position` write (by the spawner, outside
        // `on_join`) is already inside this tick's change window — the
        // stamp adds only a `WireId`, so the query then sees the entity
        // exactly once (as new-to-buckets).
        crate::common::stamp_orphans(&mut self.next_wire_id, world);
        // Clear the per-tick state (persistent containers, in place —
        // the pieces and the classification are computed lazily in the
        // broadcast phase; `tick` is current from here on).
        self.cell_changes.clear();
        self.touched.clear();
        self.born_groups.clear();
        self.frag_cache.clear();
        self.full_pieces.clear();
        self.delta_pieces.clear();
        self.exit_markers.clear();
        self.full_view.clear();
        self.group_full_emitted.clear();
        self.encoded = 0;
        self.tick = ctx.tick;

        // The dirty set (module docs, "Dirty cells"): bevy's change
        // detection flags every `Position` write — by any writer, through
        // any API (a system, `ingest`, a direct `world.entity_mut` in a
        // test or future code) — so no writer can forget to mark a cell
        // dirty: the mark lives in bevy's write path itself. The query
        // window is "writes since the end of the previous `update`"
        // (`clear_trackers` at the bottom of this method closes THIS
        // tick's window), so CONTROL-phase joins (spawn writes) are
        // inside it; CONVERT-phase writes touch `MoveTarget`, not
        // `Position`, and reach the query through the systems' resulting
        // `Position` writes; a same-value rewrite is tracked (bevy marks
        // every write, even an identical one) and degrades to a no-op
        // below through the wire comparison. Only the changed entities
        // are visited — per-tick work is proportional to the movers, not
        // to the entity count.
        let mut query =
            world.query_filtered::<(Entity, &WireId, &Position), Changed<Position>>();        for (entity, wire_id, pos) in query.iter(world) {
            let wire = wire_id.get();
            let (x, y) = (pos.x as i32, pos.y as i32);
            let new_cell = cell_of(x, y, self.cell_size);
            let is_member = self.members.contains(&entity);
            match self.last_cell.get(&entity).copied() {
                None => {
                    // New this tick (a join, or a spawn between
                    // updates): an upsert in its cell.
                    self.cell_changes
                        .entry(new_cell)
                        .or_default()
                        .updates
                        .push((wire, x, y));
                    self.buckets.entry(new_cell).or_default().insert(wire, (x, y));
                    self.last_cell.insert(entity, new_cell);
                    self.touch_cell(new_cell);
                    if is_member {
                        *self.member_counts.entry(new_cell).or_default() += 1;
                        self.member_event(new_cell, /*in:*/ true);
                    }
                }
                Some(old) if old == new_cell => {
                    // Moved inside its cell (or a move too small to
                    // change the WIRE position — quantization, module
                    // docs): a record only when the wire content
                    // actually changed.
                    let changed = self
                        .buckets
                        .get(&old)
                        .and_then(|b| b.get(&wire))
                        .is_none_or(|&(px, py)| px != x || py != y);
                    if changed {
                        self.cell_changes
                            .entry(new_cell)
                            .or_default()
                            .updates
                            .push((wire, x, y));
                        self.buckets
                            .get_mut(&old)
                            .expect("bucket invariant: tracked in last_cell")
                            .insert(wire, (x, y));
                        self.touch_cell(new_cell);
                    }
                }
                Some(old) => {
                    // A cell change: an exit in the source cell, an
                    // upsert in the target (the packet passes fix the
                    // wire order: `removed` before `entities`).
                    self.cell_changes.entry(old).or_default().exits.push(wire);
                    if let Some(b) = self.buckets.get_mut(&old) {
                        b.remove(&wire);
                        if b.is_empty() {
                            self.buckets.remove(&old);
                        }
                    }
                    self.touch_cell(old);
                    self.cell_changes
                        .entry(new_cell)
                        .or_default()
                        .updates
                        .push((wire, x, y));
                    self.buckets.entry(new_cell).or_default().insert(wire, (x, y));
                    self.last_cell.insert(entity, new_cell);
                    self.touch_cell(new_cell);
                    if is_member {
                        if let Some(n) = self.member_counts.get_mut(&old) {
                            *n -= 1;
                            if *n == 0 {
                                self.member_counts.remove(&old);
                            }
                        }
                        *self.member_counts.entry(new_cell).or_default() += 1;
                        self.member_event(old, /*in:*/ false);
                        self.member_event(new_cell, /*in:*/ true);
                    }
                }
            }
        }

        // Leavers: despawns are invisible to the change query — applied
        // from the removals parked in `on_leave` (wire id + the cell the
        // entity occupied at the end of the last `update`).
        for (entity, wire, cell) in std::mem::take(&mut self.pending_removals) {
            self.last_cell.remove(&entity);
            self.cell_changes.entry(cell).or_default().exits.push(wire);
            if let Some(b) = self.buckets.get_mut(&cell) {
                b.remove(&wire);
                if b.is_empty() {
                    self.buckets.remove(&cell);
                }
            }
            self.touch_cell(cell);
            // A parked removal is always a member's (connections own the
            // despawned entities); the join+leave-within-one-tick case
            // never parked a removal (the `last_cell` guard in
            // `on_leave`), so there is no count to undo for it.
            if let Some(n) = self.member_counts.get_mut(&cell) {
                *n -= 1;
                if *n == 0 {
                    self.member_counts.remove(&cell);
                }
            }
            self.member_event(cell, /*in:*/ false);
        }

        // The per-cell flags, the group birth, and the occupancy roll
        // (module docs, "Dirty cells") — all order-independent:
        // `prev_occupied` was frozen for the whole dirty loop (it is
        // rolled only here, against the final bucket state), and the
        // member arithmetic reconstructs the before-tick count from the
        // net events (`now − in + out`), so a same-tick exit+entry into
        // the same cell cannot fake a birth. The pass iterates exactly
        // the touched cells — O(movers), never O(occupied cells).
        for (c, t) in self.touched.iter() {
            let occupied_now = self.buckets.contains_key(c);
            let occupied_prev = self.prev_occupied.contains(c);
            let ch = self
                .cell_changes
                .get_mut(c)
                .expect("a touched cell has a change entry");
            ch.appeared = occupied_now && !occupied_prev;
            ch.exited = occupied_prev && !occupied_now;
            let now = self.member_counts.get(c).copied().unwrap_or(0);
            let before = now.wrapping_sub(t.member_in).wrapping_add(t.member_out);
            debug_assert!(
                before.saturating_add(t.member_in) >= t.member_out,
                "member count went negative for {c:?}"
            );
            if before == 0 && now > 0 {
                self.born_groups.insert(*c);
            }
            if occupied_now {
                self.prev_occupied.insert(*c);
            } else {
                self.prev_occupied.remove(c);
            }
        }

        // Close this tick's change window: bevy's change tick advances
        // here, so the NEXT `update`'s query sees exactly the writes
        // made since now — the next CONTROL phase's join spawns
        // included. Standalone bevy does not call this on its own
        // (it is the system-scheduler's job, and there is none here —
        // see `docs/DESIGN.md` §7).
        world.clear_trackers();
    }
}

#[cfg(test)]
mod tests {
    //! Logic-level AOI tests (precise, direct `AoiRoom` calls; they need
    //! the room's private bookkeeping, so they live here rather than in
    //! `tests/aoi.rs` — the room-actor-level behaviour, delta streams,
    //! client views, and the loss-recovery bound live in `tests/aoi.rs`
    //! and `tests/delta_aoi.rs`). `cell_size = 20` ⇒ (0,0)/(15,0) share
    //! `Cell(0,0)`; (45,0) is `Cell(2,0)` (inside the 3×3 centered on
    //! `Cell(1,0)`); (100,0) is `Cell(5,0)` (far).

    use std::collections::BTreeSet;
    use std::time::Duration;

    use bevy_ecs::prelude::World;
    use gsb_core::id::{ConnectionId, RoomId};
    use gsb_core::room::TickCtx;
    use crate::game::{CellExit, WorldSnapshot};
    use prost::Message;

    use crate::components::{Position, Speed, DEFAULT_SPEED};

    use super::*;

    fn ctx(tick: u64) -> TickCtx {
        TickCtx {
            room: RoomId(1),
            tick,
            dt: Duration::from_secs_f64(1.0 / 30.0),
        }
    }

    /// Join a player (wire id assigned) and move its entity to an exact
    /// position for a deterministic cell placement. Returns the wire id.
    fn place(world: &mut World, room: &mut AoiRoom, conn: ConnectionId, x: f32, y: f32) -> u64 {
        let wire = room.on_join(world, conn);
        let entity = *room.conn_entity.get(&conn).expect("conn registered");
        world.entity_mut(entity).insert(Position { x, y });
        wire
    }

    fn decode(out: &bytes::BytesMut) -> WorldSnapshot {
        WorldSnapshot::decode(out.as_ref()).expect("snapshot payload")
    }

    fn ids(snap: &WorldSnapshot) -> BTreeSet<u64> {
        snap.entities.iter().map(|e| e.entity).collect()
    }

    /// A cell's full piece carries exactly the cell's records; a fresh
    /// group's packet is a full (delta=false) of its 3×3.
    #[test]
    fn aoi_block_contains_near_not_far() {
        let mut world = World::new();
        let mut room = AoiRoom::new(20.0);

        let a = place(&mut world, &mut room, ConnectionId(1), 0.0, 0.0); // Cell(0,0)
        let b = place(&mut world, &mut room, ConnectionId(2), 15.0, 0.0); // Cell(0,0)
        let c = place(&mut world, &mut room, ConnectionId(3), 100.0, 0.0); // Cell(5,0)
        room.update(&mut world, &ctx(1));

        let mut out = bytes::BytesMut::new();
        assert!(room.snapshot(&mut world, &ctx(1), &Cell(0, 0), &mut out));
        let snap = decode(&out);
        assert!(!snap.delta, "a fresh group's first packet is a full");
        let near = ids(&snap);
        assert!(near.contains(&a) && near.contains(&b), "co-residents visible: {near:?}");
        assert!(!near.contains(&c), "far cell must not be visible: {near:?}");

        let mut out2 = bytes::BytesMut::new();
        assert!(room.snapshot(&mut world, &ctx(1), &Cell(5, 0), &mut out2));
        let far = ids(&decode(&out2));
        assert!(far.contains(&c), "C sees itself: {far:?}");
        assert!(!far.contains(&a) && !far.contains(&b), "far C does not see A/B: {far:?}");
    }

    /// Cell transition: an entity crossing a boundary changes group; the
    /// new cell's DELTA carries it as an update, the old cell's delta
    /// carries its exit (or a cell exit when the cell becomes empty) —
    /// and its wire identity is unchanged throughout.
    #[test]
    fn aoi_cell_transition_block_and_identity() {
        let mut world = World::new();
        let mut room = AoiRoom::new(20.0);

        let a = place(&mut world, &mut room, ConnectionId(1), 0.0, 0.0); // Cell(0,0)
        let b = place(&mut world, &mut room, ConnectionId(2), 60.0, 0.0); // Cell(3,0)
        assert_ne!(a, b);

        room.update(&mut world, &ctx(1));
        let mut out = bytes::BytesMut::new();
        assert!(room.snapshot(&mut world, &ctx(1), &Cell(0, 0), &mut out));
        assert!(ids(&decode(&out)).contains(&a));

        // A moves into B's cell (Cell(3,0)); identity must be preserved.
        let entity_a = *room.conn_entity.get(&ConnectionId(1)).unwrap();
        world.entity_mut(entity_a).insert(Position { x: 60.0, y: 0.0 });
        room.update(&mut world, &ctx(2));

        // Group Cell(3,0) is established (B since tick 1): its packet is
        // a delta; A is an update in it (B is unchanged and stays in the
        // client's baseline — the delta does not re-carry it).
        let mut out_b = bytes::BytesMut::new();
        assert!(
            room.snapshot(&mut world, &ctx(2), &Cell(3, 0), &mut out_b),
            "A's arrival re-emits (delta)"
        );
        let snap_b = decode(&out_b);
        assert!(snap_b.delta, "an established group ships a delta");
        let now_b = ids(&snap_b);
        assert!(now_b.contains(&a), "A is an update in the new cell's delta: {now_b:?}");
        assert!(snap_b.removed.iter().all(|w| *w != a), "A is not exited");

        // Group Cell(0,0): A left its only resident cell → the cell
        // became empty → ONE cell-exit record (not per entity), no
        // entities re-carried.
        let mut out_a = bytes::BytesMut::new();
        assert!(
            room.snapshot(&mut world, &ctx(2), &Cell(0, 0), &mut out_a),
            "A's departure re-emits (cell exit)"
        );
        let snap_a = decode(&out_a);
        assert!(snap_a.delta);
        assert!(snap_a.entities.is_empty(), "no entity records: {snap_a:?}");
        let exits: BTreeSet<(i32, i32)> = snap_a
            .cell_exits
            .iter()
            .map(|e| (e.x, e.y))
            .collect();
        assert!(
            exits.contains(&(0, 0)),
            "the emptied cell is exited as one record: {exits:?}"
        );
        // Identity preserved across the cell move (the same wire id in
        // both cells' records).
        assert!(ids(&decode(&out_b)).contains(&a), "A keeps its wire id");
    }

    /// Late join: a player entering an already-populated cell's group
    /// gets its full visibility block — now as a one-shot **private**
    /// full (the group stays in delta mode for everyone; the group's own
    /// packet is a delta and does not carry the co-residents again).
    #[test]
    fn aoi_late_join_sees_full_visibility_block() {
        let mut world = World::new();
        let mut room = AoiRoom::new(20.0);

        let a = place(&mut world, &mut room, ConnectionId(1), 0.0, 0.0);
        let d = place(&mut world, &mut room, ConnectionId(2), 15.0, 0.0);
        room.update(&mut world, &ctx(1));
        let mut out = bytes::BytesMut::new();
        assert!(room.snapshot(&mut world, &ctx(1), &Cell(0, 0), &mut out));

        // B joins the same cell late (the group is established: its
        // packet is a delta that does NOT re-carry the co-residents).
        let b = place(&mut world, &mut room, ConnectionId(3), 5.0, 0.0);
        room.update(&mut world, &ctx(2));

        let mut out2 = bytes::BytesMut::new();
        // The group's own packet: silent for the unchanged residents
        // (B is a new entity → its cell's content changed → it is an
        // update; A and D are in the baseline, not re-carried).
        assert!(
            room.snapshot(&mut world, &ctx(2), &Cell(0, 0), &mut out2),
            "B's spawn re-emits (delta update)"
        );
        let group_delta = decode(&out2);
        assert!(group_delta.delta);
        assert!(
            ids(&group_delta).contains(&b),
            "the group delta carries B (new): {:?}",
            ids(&group_delta)
        );

        // The one-shot private full: B's entire visibility block
        // (co-residents + itself, full mode).
        let mut priv_out = bytes::BytesMut::new();
        assert!(
            room.private(&mut world, ConnectionId(3), &Cell(0, 0), &mut priv_out),
            "a late joiner receives the one-shot full"
        );
        let full = crate::game::Private::decode(priv_out.as_ref()).expect("private frame");
        let snap = match full.payload {
            Some(crate::game::private::Payload::Snapshot(s)) => s,
            other => panic!("expected the snapshot oneof, got {other:?}"),
        };
        assert!(!snap.delta, "the one-shot full is a full snapshot");
        let seen = ids(&snap);
        assert!(seen.contains(&a) && seen.contains(&d), "sees co-residents: {seen:?}");
        assert!(seen.contains(&b), "sees itself: {seen:?}");

        // A second private call the same tick (or a later tick with no
        // group change): no full again (only an ack, if any).
        let mut priv_out2 = bytes::BytesMut::new();
        assert!(
            !room.private(&mut world, ConnectionId(3), &Cell(0, 0), &mut priv_out2),
            "the one-shot full is one-shot"
        );
    }

    /// Broadcast set: an entity spawned with a `Position` but no `WireId`
    /// (a bullet/NPC, not via `on_join`) is stamped in `update` and
    /// appears in its cell's full. The broadcast set is exactly "has a
    /// `Position`".
    #[test]
    fn aoi_broadcast_set_position_is_stamped() {
        let mut world = World::new();
        let mut room = AoiRoom::new(20.0);

        let a = place(&mut world, &mut room, ConnectionId(1), 0.0, 0.0);
        let _orphan = world
            .spawn((Position { x: 3.0, y: 3.0 }, Speed(DEFAULT_SPEED)))
            .id();
        room.update(&mut world, &ctx(1));

        let mut out = bytes::BytesMut::new();
        assert!(room.snapshot(&mut world, &ctx(1), &Cell(0, 0), &mut out));
        let snap = decode(&out);
        let ids = ids(&snap);
        assert!(ids.contains(&a), "resident present: {ids:?}");
        assert_eq!(ids.len(), 2, "orphan stamped and broadcast (2 entities): {ids:?}");
        assert!(!ids.contains(&0), "wire ids start at 1");
    }

    /// "No change" contract: a 3×3 whose content is identical across two
    /// ticks is not re-encoded (silence writes no bytes); a position
    /// change flips the cell to a delta.
    #[test]
    fn aoi_no_change_when_block_static() {
        let mut world = World::new();
        let mut room = AoiRoom::new(20.0);

        let _ = place(&mut world, &mut room, ConnectionId(1), 7.0, 9.0); // no MoveTarget
        let cell = Cell(0, 0);

        room.update(&mut world, &ctx(1));
        let mut out1 = bytes::BytesMut::new();
        assert!(room.snapshot(&mut world, &ctx(1), &cell, &mut out1), "first emit (full)");

        room.update(&mut world, &ctx(2));
        let mut out2 = bytes::BytesMut::new();
        assert!(
            !room.snapshot(&mut world, &ctx(2), &cell, &mut out2),
            "static 3×3 silent"
        );
        assert!(out2.is_empty(), "no bytes written on silence");

        let entity = *room.conn_entity.get(&ConnectionId(1)).unwrap();
        world.entity_mut(entity).insert(Position { x: 7.0, y: 19.0 });
        room.update(&mut world, &ctx(3));
        let mut out3 = bytes::BytesMut::new();
        assert!(
            room.snapshot(&mut world, &ctx(3), &cell, &mut out3),
            "movement re-emits (delta)"
        );
        let snap3 = decode(&out3);
        assert!(snap3.delta, "the re-emit is a delta");
        assert_eq!(snap3.entities.len(), 1, "the delta carries the one mover");
        assert_eq!(snap3.removed.len(), 0, "no exits");
    }

    /// The keep-alive path for an unchanged group is a freshly encoded
    /// FULL of the group's view (not a re-send of the last delta).
    #[test]
    fn aoi_keepalive_ships_fresh_full() {
        let mut world = World::new();
        let mut room = AoiRoom::new(20.0);

        let a = place(&mut world, &mut room, ConnectionId(1), 0.0, 0.0);
        let b = place(&mut world, &mut room, ConnectionId(2), 15.0, 0.0);
        room.update(&mut world, &ctx(1));
        let cell = Cell(0, 0);
        let mut out1 = bytes::BytesMut::new();
        assert!(room.snapshot(&mut world, &ctx(1), &cell, &mut out1));

        // Static: the group is silent, so the keep-alive fires.
        room.update(&mut world, &ctx(2));
        let last = out1.clone().freeze();
        let mut ka = bytes::BytesMut::new();
        assert!(
            room.keepalive(&mut world, &ctx(2), &cell, Some(&last), &mut ka),
            "the delta-mode logic overrides the keep-alive"
        );
        let snap = decode(&ka);
        assert!(!snap.delta, "the keep-alive payload is a full");
        let seen = ids(&snap);
        assert!(seen.contains(&a) && seen.contains(&b), "the full carries the view: {seen:?}");
    }

    /// The full/delta decision is per (group, cell): two groups sharing a
    /// cell that becomes empty get the SAME single cell-exit record, and
    /// a cell that re-appears is delivered as full records to groups
    /// without a baseline.
    #[test]
    fn aoi_cell_exit_is_one_record_and_shared() {
        let mut world = World::new();
        let mut room = AoiRoom::new(20.0);

        // P1 in Cell(0,0), P2 in Cell(1,0): both groups' 3×3 include
        // Cell(0,1) (the test cell below).
        let _p1 = place(&mut world, &mut room, ConnectionId(1), 10.0, 0.0);
        let _p2 = place(&mut world, &mut room, ConnectionId(2), 30.0, 0.0);
        room.update(&mut world, &ctx(1));

        // Tick 2: an entity in Cell(0,1) — inside BOTH groups' 3×3.
        let e = world
            .spawn((Position { x: 5.0, y: 25.0 }, Speed(DEFAULT_SPEED)))
            .id();
        room.update(&mut world, &ctx(2));

        // Tick 3: the entity leaves Cell(0,1) (to a far cell) — the cell
        // becomes empty; BOTH groups' packets carry exactly one
        // cell-exit record for it (one record, not one per entity), and
        // no entity records for it at all.
        let ent = world.entity(e).get::<crate::components::WireId>().expect("stamped").get();
        world.entity_mut(e).insert(Position { x: 5.0, y: 400.0 });
        room.update(&mut world, &ctx(3));

        for group in [Cell(0, 0), Cell(1, 0)] {
            let mut out = bytes::BytesMut::new();
            assert!(
                room.snapshot(&mut world, &ctx(3), &group, &mut out),
                "the departure re-emits for group {group:?}"
            );
            let snap = decode(&out);
            assert!(snap.delta);
            assert_eq!(
                snap.cell_exits.len(),
                1,
                "one cell-exit record for the emptied cell (group {group:?}): {snap:?}"
            );
            let exit: &CellExit = &snap.cell_exits[0];
            assert_eq!((exit.x, exit.y), (0, 1), "the exited cell is named: {snap:?}");
            assert!(
                snap.entities.iter().all(|r| r.entity != ent),
                "the departed entity is not re-carried (group {group:?})"
            );
        }
    }

    /// The encoded-records metric counts each entity's piece once per
    /// tick (the overlap collapse): two groups sharing one populated cell
    /// encode that cell's records once, not twice.
    #[test]
    fn aoi_encoding_is_once_per_cell_not_per_group() {
        let mut world = World::new();
        let mut room = AoiRoom::new(20.0);

        // Two groups (two cells) sharing the populated Cell(0,0).
        let _a = place(&mut world, &mut room, ConnectionId(1), 10.0, 0.0); // Cell(0,0)
        let _b = place(&mut world, &mut room, ConnectionId(2), 30.0, 0.0); // Cell(1,0)
        let npc = world
            .spawn((Position { x: 5.0, y: 5.0 }, Speed(DEFAULT_SPEED)))
            .id();
        room.update(&mut world, &ctx(1));

        // Tick 1: both groups are fresh → both fulls encode Cell(0,0)'s
        // two records ONCE (the full piece is shared).
        let mut out = bytes::BytesMut::new();
        assert!(room.snapshot(&mut world, &ctx(1), &Cell(0, 0), &mut out));
        assert!(room.snapshot(&mut world, &ctx(1), &Cell(1, 0), &mut out));
        assert_eq!(room.encoded_records(), 3, "2 shared + 1 exclusive, each once");

        // Tick 2: static → silence, nothing encoded.
        room.update(&mut world, &ctx(2));
        assert!(!room.snapshot(&mut world, &ctx(2), &Cell(0, 0), &mut out));
        assert!(!room.snapshot(&mut world, &ctx(2), &Cell(1, 0), &mut out));
        assert_eq!(room.encoded_records(), 0, "silence encodes nothing");

        // Tick 3: the NPC moves within its cell → one delta piece for
        // Cell(0,0) (shared by both groups): 1 record encoded.
        world.entity_mut(npc).insert(Position { x: 8.0, y: 5.0 });
        room.update(&mut world, &ctx(3));
        assert!(room.snapshot(&mut world, &ctx(3), &Cell(0, 0), &mut out));
        assert!(room.snapshot(&mut world, &ctx(3), &Cell(1, 0), &mut out));
        assert_eq!(room.encoded_records(), 1, "the shared cell's delta is encoded once");
    }

    /// The pieces concatenate into a decodable `WorldSnapshot`: header +
    /// several cells' pre-encoded blocks decode to the union (the
    /// verified merge property the design relies on).
    #[test]
    fn aoi_concatenated_pieces_are_valid_protobuf() {
        let mut world = World::new();
        let mut room = AoiRoom::new(20.0);

        // One entity per cell in three adjacent cells.
        let a = place(&mut world, &mut room, ConnectionId(1), 5.0, 5.0); // Cell(0,0)
        let b = place(&mut world, &mut room, ConnectionId(2), 25.0, 5.0); // Cell(1,0)
        let c = place(&mut world, &mut room, ConnectionId(3), 45.0, 5.0); // Cell(2,0)
        room.update(&mut world, &ctx(1));

        let mut out = bytes::BytesMut::new();
        assert!(room.snapshot(&mut world, &ctx(1), &Cell(1, 0), &mut out));
        let snap = decode(&out);
        let seen = ids(&snap);
        assert!(
            seen.contains(&a) && seen.contains(&b) && seen.contains(&c),
            "the group's packet is the union of the three cells' blocks: {seen:?}"
        );
        assert_eq!(snap.entities.len(), 3, "exactly the union, no duplicates: {snap:?}");
    }

    // ── Cache-invalidation tests (round item (a)): the per-tick
    //    classification/piece caches must never serve a stale answer —
    //    across ticks (different content → different blocks) or within
    //    the silent/delta alternation (a stale delta must not be
    //    re-served, and two groups asking the same cell get the same
    //    block).

    /// (a) two consecutive ticks with different content produce
    /// different blocks: tick N+1's piece is never tick N's cached
    /// piece (the per-tick caches are cleared in `update`).
    #[test]
    fn aoi_tick_cache_no_stale_block() {
        let mut world = World::new();
        let mut room = AoiRoom::new(20.0);
        let _a = place(&mut world, &mut room, ConnectionId(1), 0.0, 0.0); // Cell(0,0)
        let ent = *room.conn_entity.get(&ConnectionId(1)).unwrap();

        room.update(&mut world, &ctx(1));
        let mut out = bytes::BytesMut::new();
        assert!(room.snapshot(&mut world, &ctx(1), &Cell(0, 0), &mut out)); // fresh full

        // Tick 2: the entity moves within its cell → a delta with (15,0).
        world.entity_mut(ent).insert(Position { x: 15.0, y: 0.0 });
        room.update(&mut world, &ctx(2));
        let mut out2 = bytes::BytesMut::new();
        assert!(room.snapshot(&mut world, &ctx(2), &Cell(0, 0), &mut out2));
        let s2 = decode(&out2);
        assert!(s2.delta);
        assert_eq!(s2.entities.len(), 1, "exactly the moved record: {s2:?}");
        assert_eq!((s2.entities[0].x, s2.entities[0].y), (15, 0));
        assert_eq!(room.encoded_records(), 1);

        // Tick 3: it moves again → a DIFFERENT block ((10,0)) — tick 2's
        // cached piece/classification must not be replayed.
        world.entity_mut(ent).insert(Position { x: 10.0, y: 0.0 });
        room.update(&mut world, &ctx(3));
        let mut out3 = bytes::BytesMut::new();
        assert!(room.snapshot(&mut world, &ctx(3), &Cell(0, 0), &mut out3));
        let s3 = decode(&out3);
        assert!(s3.delta);
        assert_eq!(s3.entities.len(), 1, "exactly the moved record: {s3:?}");
        assert_eq!(
            (s3.entities[0].x, s3.entities[0].y),
            (10, 0),
            "tick 3 must not serve tick 2's cached block"
        );
        assert_eq!(room.encoded_records(), 1);
    }

    /// (a) a cell that goes silent after a delta tick ships nothing —
    /// the stale delta is not re-served; the keep-alive full carries the
    /// CURRENT content (never a stale piece).
    #[test]
    fn aoi_silent_delta_silent_no_stale() {
        let mut world = World::new();
        let mut room = AoiRoom::new(20.0);
        let _a = place(&mut world, &mut room, ConnectionId(1), 0.0, 0.0); // Cell(0,0)
        let ent = *room.conn_entity.get(&ConnectionId(1)).unwrap();

        room.update(&mut world, &ctx(1));
        let mut out = bytes::BytesMut::new();
        assert!(room.snapshot(&mut world, &ctx(1), &Cell(0, 0), &mut out)); // fresh full

        // Tick 2: silence — an established group ships NOTHING.
        room.update(&mut world, &ctx(2));
        let mut out2 = bytes::BytesMut::new();
        assert!(
            !room.snapshot(&mut world, &ctx(2), &Cell(0, 0), &mut out2),
            "silence ships nothing (no stale full, no delta)"
        );
        assert!(out2.is_empty());
        assert_eq!(room.encoded_records(), 0);

        // Tick 3: a delta tick — the silence of tick 2 must not
        // suppress the cell's (fresh) piece.
        world.entity_mut(ent).insert(Position { x: 19.0, y: 0.0 });
        room.update(&mut world, &ctx(3));
        let mut out3 = bytes::BytesMut::new();
        assert!(room.snapshot(&mut world, &ctx(3), &Cell(0, 0), &mut out3));
        let s3 = decode(&out3);
        assert!(s3.delta);
        assert_eq!(s3.entities.len(), 1);
        assert_eq!((s3.entities[0].x, s3.entities[0].y), (19, 0));
        assert_eq!(room.encoded_records(), 1);

        // Tick 4: silence again — the stale tick-3 delta is not
        // replayed, and the keep-alive full carries the current content.
        room.update(&mut world, &ctx(4));
        let mut out4 = bytes::BytesMut::new();
        assert!(
            !room.snapshot(&mut world, &ctx(4), &Cell(0, 0), &mut out4),
            "the stale delta must not be re-served"
        );
        assert_eq!(room.encoded_records(), 0);
        let mut ka = bytes::BytesMut::new();
        assert!(room.keepalive(&mut world, &ctx(4), &Cell(0, 0), None, &mut ka));
        let s4 = decode(&ka);
        assert!(!s4.delta, "the keep-alive ships a full");
        assert_eq!(s4.entities.len(), 1);
        assert_eq!(
            (s4.entities[0].x, s4.entities[0].y),
            (19, 0),
            "the full carries the current content, not a stale piece"
        );
    }

    /// (a) two different groups that both see the same cell in the same
    /// tick get the SAME block for it: content-identical records, and
    /// the piece is encoded exactly once (shared by reference).
    #[test]
    fn aoi_two_groups_same_cell_same_block() {
        let mut world = World::new();
        let mut room = AoiRoom::new(20.0);

        // Two member groups (Cell(1,0) and Cell(3,0)) whose 3×3s overlap
        // in Cell(2,0); an NPC lives in Cell(2,0).
        let _m1 = place(&mut world, &mut room, ConnectionId(1), 20.0, 0.0); // Cell(1,0)
        let _m2 = place(&mut world, &mut room, ConnectionId(2), 60.0, 0.0); // Cell(3,0)
        let npc = world
            .spawn((Position { x: 40.0, y: 0.0 }, Speed(DEFAULT_SPEED)))
            .id(); // Cell(2,0)
        room.update(&mut world, &ctx(1));

        // Tick 2: the NPC moves within Cell(2,0) — both groups' deltas
        // must carry the identical block for that cell.
        world.entity_mut(npc).insert(Position { x: 45.0, y: 0.0 });
        room.update(&mut world, &ctx(2));

        let mut out_a = bytes::BytesMut::new();
        let mut out_b = bytes::BytesMut::new();
        assert!(room.snapshot(&mut world, &ctx(2), &Cell(1, 0), &mut out_a));
        assert!(room.snapshot(&mut world, &ctx(2), &Cell(3, 0), &mut out_b));
        let s_a = decode(&out_a);
        let s_b = decode(&out_b);
        assert_eq!(s_a.entities.len(), 1, "group A's delta carries only the shared cell's mover: {s_a:?}");
        assert_eq!(s_b.entities.len(), 1, "group B's delta carries only the shared cell's mover: {s_b:?}");
        assert_eq!(
            (s_a.entities[0].entity, s_a.entities[0].x, s_a.entities[0].y),
            (s_b.entities[0].entity, s_b.entities[0].x, s_b.entities[0].y),
            "same cell, same tick → identical block"
        );
        assert_eq!(room.encoded_records(), 1, "one encoding, not one per group");
    }

    // ── Change-detection tests (round item (b)): the dirty marking is
    //    structural (bevy's write path — no `bump()` discipline), the
    //    change list is the diff (partial deltas, quantization no-ops),
    //    and the bookkeeping it maintains (leaves, births, same-tick
    //    churn) is order-independent and leak-free.

    /// (b) structural dirty marking: a `Position` written by a writer
    /// the room has no hook into (a direct `world.entity_mut` — no
    /// system, no `MOVE_TO`, no invalidation call) still produces the
    /// correct delta: the mark is set inside bevy's write path, so it
    /// cannot be forgotten by any present or future writer.
    #[test]
    fn aoi_structural_dirty_direct_write() {
        let mut world = World::new();
        let mut room = AoiRoom::new(20.0);
        let _m = place(&mut world, &mut room, ConnectionId(1), 0.0, 0.0); // Cell(0,0)
        // Two third-party entities the room never sees through
        // on_join/ingest (a co-resident keeps the source cell
        // non-empty, so the exit takes the per-entity `removed` path —
        // the whole-cell `CellExit` path is covered by
        // `aoi_leave_removal_in_delta_and_cell_exit`).
        let ghost = world
            .spawn((Position { x: 100.0, y: 0.0 }, Speed(DEFAULT_SPEED)))
            .id(); // Cell(5,0)
        world.spawn((Position { x: 110.0, y: 0.0 }, Speed(DEFAULT_SPEED))); // Cell(5,0)
        room.update(&mut world, &ctx(1));
        let ghost_wire = world.entity(ghost).get::<WireId>().expect("stamped").get();

        // The third-party writer moves the ghost across cells — the room
        // has no hook for this write; the delta must still carry the
        // exit (source cell) and the arrival (target cell).
        world.entity_mut(ghost).insert(Position { x: 125.0, y: 0.0 }); // Cell(6,0)
        room.update(&mut world, &ctx(2));

        let mut out = bytes::BytesMut::new();
        assert!(room.snapshot(&mut world, &ctx(2), &Cell(5, 0), &mut out));
        let s = decode(&out);
        assert!(s.delta);
        assert!(
            s.removed.contains(&ghost_wire),
            "the exit is recorded without any room hook: {s:?}"
        );

        let mut out2 = bytes::BytesMut::new();
        assert!(room.snapshot(&mut world, &ctx(2), &Cell(6, 0), &mut out2));
        let s2 = decode(&out2);
        assert!(
            s2
                .entities
                .iter()
                .any(|e| e.entity == ghost_wire && (e.x, e.y) == (125, 0)),
            "the arrival is recorded: {s2:?}"
        );
    }

    /// (b) partial delta: a cell with five records of which ONE changed
    /// ships exactly that one record — the other four are neither
    /// re-carried nor re-encoded (the change list is the diff; no
    /// per-cell content comparison runs).
    #[test]
    fn aoi_partial_delta_only_mover_recorded() {
        let mut world = World::new();
        let mut room = AoiRoom::new(20.0);
        // Five members, one cell (Cell(0,0): x = 3, 6, 9, 12, 15).
        let ws: Vec<u64> = (1..=5)
            .map(|i| place(&mut world, &mut room, ConnectionId(i), (i as f32) * 3.0, 0.0))
            .collect();
        room.update(&mut world, &ctx(1));
        let mut out = bytes::BytesMut::new();
        assert!(room.snapshot(&mut world, &ctx(1), &Cell(0, 0), &mut out));
        assert_eq!(ids(&decode(&out)).len(), 5, "the fresh full carries all five");
        assert_eq!(room.encoded_records(), 5);

        // Only member 3 (x=9) moves — a direct write (no MOVE_TO, no
        // system).
        let ent = *room.conn_entity.get(&ConnectionId(3)).unwrap();
        world.entity_mut(ent).insert(Position { x: 18.0, y: 0.0 }); // still Cell(0,0)
        room.update(&mut world, &ctx(2));
        let mut out2 = bytes::BytesMut::new();
        assert!(room.snapshot(&mut world, &ctx(2), &Cell(0, 0), &mut out2));
        let s2 = decode(&out2);
        assert!(s2.delta);
        assert_eq!(s2.entities.len(), 1, "exactly the mover's record: {s2:?}");
        assert_eq!(s2.entities[0].entity, ws[2]);
        assert_eq!((s2.entities[0].x, s2.entities[0].y), (18, 0));
        assert!(s2.removed.is_empty(), "no exits: {s2:?}");
        assert_eq!(
            room.encoded_records(),
            1,
            "one encoding — the other four records are not re-encoded"
        );
    }

    /// (b) quantization: a move that leaves the WIRE position (i32
    /// truncation) untouched produces no record — the cell stays silent
    /// (no stale delta leaks) while the f32 world keeps moving; the
    /// first wire-unit change does produce the record.
    #[test]
    fn aoi_quantized_move_no_wire_change_no_record() {
        let mut world = World::new();
        let mut room = AoiRoom::new(20.0);
        let _m = place(&mut world, &mut room, ConnectionId(1), 0.5, 0.5); // wire (0,0), Cell(0,0)
        room.update(&mut world, &ctx(1));
        let mut out = bytes::BytesMut::new();
        assert!(room.snapshot(&mut world, &ctx(1), &Cell(0, 0), &mut out));

        // The f32 position changes; the wire position (0,0) does not.
        let ent = *room.conn_entity.get(&ConnectionId(1)).unwrap();
        world.entity_mut(ent).insert(Position { x: 0.9, y: 0.5 });
        room.update(&mut world, &ctx(2));
        let mut out2 = bytes::BytesMut::new();
        assert!(
            !room.snapshot(&mut world, &ctx(2), &Cell(0, 0), &mut out2),
            "an unchanged wire position is a no-op: the cell is silent, no stale delta"
        );
        assert_eq!(room.encoded_records(), 0);

        // Crossing the wire boundary (x: 0.9 → 1.0) flips the record.
        world.entity_mut(ent).insert(Position { x: 1.0, y: 0.5 });
        room.update(&mut world, &ctx(3));
        let mut out3 = bytes::BytesMut::new();
        assert!(room.snapshot(&mut world, &ctx(3), &Cell(0, 0), &mut out3));
        let s3 = decode(&out3);
        assert!(s3.delta);
        assert_eq!(s3.entities.len(), 1);
        assert_eq!((s3.entities[0].x, s3.entities[0].y), (1, 0));
        assert_eq!(room.encoded_records(), 1);
    }

    /// (b) the despawn path: a leave is not a component write — the
    /// removal is parked in `on_leave` and applied by `update`. The
    /// cell's delta carries the leaver's wire id in `removed`; when the
    /// cell empties, one `CellExit` supersedes the per-entity records.
    #[test]
    fn aoi_leave_removal_in_delta_and_cell_exit() {
        let mut world = World::new();
        let mut room = AoiRoom::new(20.0);
        let w1 = place(&mut world, &mut room, ConnectionId(1), 0.0, 0.0); // Cell(0,0)
        let w2 = place(&mut world, &mut room, ConnectionId(2), 10.0, 0.0); // Cell(0,0)
        room.update(&mut world, &ctx(1));
        let mut out = bytes::BytesMut::new();
        assert!(room.snapshot(&mut world, &ctx(1), &Cell(0, 0), &mut out));
        assert_eq!(ids(&decode(&out)).len(), 2);
        assert_eq!(room.encoded_records(), 2);

        // One leaves: the delta carries exactly its wire id in `removed`
        // (the remaining entity is not re-carried).
        room.on_leave(&mut world, ConnectionId(1));
        room.update(&mut world, &ctx(2));
        let mut out2 = bytes::BytesMut::new();
        assert!(room.snapshot(&mut world, &ctx(2), &Cell(0, 0), &mut out2));
        let s2 = decode(&out2);
        assert!(s2.delta);
        assert_eq!(s2.removed.len(), 1, "the leaver's wire id is removed: {s2:?}");
        assert_eq!(s2.removed[0], w1);
        assert!(!s2.removed.contains(&w2), "the remaining entity is NOT removed: {s2:?}");
        assert!(s2.entities.is_empty(), "the remaining entity is not re-carried: {s2:?}");
        assert_eq!(room.encoded_records(), 0, "exits are not 'encoded records'");

        // The last one leaves: a `CellExit` supersedes the per-entity
        // record (the client forgets the whole cell in one record).
        room.on_leave(&mut world, ConnectionId(2));
        room.update(&mut world, &ctx(3));
        let mut out3 = bytes::BytesMut::new();
        assert!(room.snapshot(&mut world, &ctx(3), &Cell(0, 0), &mut out3));
        let s3 = decode(&out3);
        assert_eq!(s3.cell_exits.len(), 1, "one cell-exit record: {s3:?}");
        assert_eq!((s3.cell_exits[0].x, s3.cell_exits[0].y), (0, 0));
        assert!(s3.removed.is_empty(), "the cell-exit supersedes the entity records: {s3:?}");
        assert!(s3.entities.is_empty());
    }

    /// (b) the birth rule's generalization (module docs, "Dirty cells"):
    /// a cell that already holds non-member content (an NPC) and gains
    /// its first MEMBER this tick is a fresh group — the member is
    /// baselined by the group's own full (batch-ordered ahead of the
    /// private frame), so the one-shot private full is skipped. (The
    /// spec's literal bucket-diff formulation would miss this birth —
    /// the cell was already in the previous buckets — leaving the member
    /// to the private full; both paths baseline correctly, and the
    /// member-count rule is the one that also covers the general case.)
    #[test]
    fn aoi_member_join_npc_cell_born_full() {
        let mut world = World::new();
        let mut room = AoiRoom::new(20.0);

        // Tick 1: an NPC-only cell (no members anywhere yet).
        let npc = world
            .spawn((Position { x: 40.0, y: 0.0 }, Speed(DEFAULT_SPEED)))
            .id(); // Cell(2,0)
        room.update(&mut world, &ctx(1));

        // Tick 2: a member joins and lands in the NPC's cell.
        let m = place(&mut world, &mut room, ConnectionId(7), 44.0, 0.0); // Cell(2,0)
        room.update(&mut world, &ctx(2));
        assert!(
            room.born_groups.contains(&Cell(2, 0)),
            "a member joining an NPC-held cell is a fresh group (member count 0 → 1)"
        );
        let mut out = bytes::BytesMut::new();
        assert!(room.snapshot(&mut world, &ctx(2), &Cell(2, 0), &mut out));
        let s = decode(&out);
        assert!(!s.delta, "the fresh group's first packet is a full");
        let seen = ids(&s);
        let npc_wire = world.entity(npc).get::<WireId>().expect("stamped").get();
        assert!(
            seen.contains(&m) && seen.contains(&npc_wire),
            "the full carries member + NPC: {seen:?}"
        );
        // The member is baselined by that full: no private frame.
        let mut pbuf = bytes::BytesMut::new();
        assert!(
            !room.private(&mut world, ConnectionId(7), &Cell(2, 0), &mut pbuf),
            "the group's full already baselined the member — no private frame"
        );
        assert!(room.conn_view.contains_key(&ConnectionId(7)));
    }

    /// (b) a join+leave within one tick: the entity never enters
    /// `last_cell` (no `update` ran between the two), so `on_leave`
    /// parks no removal, the dirty query never sees the despawned
    /// entity, and no bookkeeping is left behind — counts, buckets, and
    /// the established group's stream are exactly as before.
    #[test]
    fn aoi_join_leave_same_tick_inert() {
        let mut world = World::new();
        let mut room = AoiRoom::new(20.0);
        let _m = place(&mut world, &mut room, ConnectionId(1), 0.0, 0.0); // Cell(0,0)
        room.update(&mut world, &ctx(1));
        let mut out = bytes::BytesMut::new();
        assert!(room.snapshot(&mut world, &ctx(1), &Cell(0, 0), &mut out)); // fresh full

        // A connection that joins AND leaves before the next update.
        room.on_join(&mut world, ConnectionId(9));
        room.on_leave(&mut world, ConnectionId(9));
        assert!(
            room.pending_removals.is_empty(),
            "no removal to park (the entity was never bucketed)"
        );

        room.update(&mut world, &ctx(2));
        assert_eq!(
            *room.member_counts.get(&Cell(0, 0)).expect("member count"),
            1,
            "the member counts are intact"
        );
        let mut out2 = bytes::BytesMut::new();
        assert!(
            !room.snapshot(&mut world, &ctx(2), &Cell(0, 0), &mut out2),
            "the phantom join produced no delta"
        );
        assert_eq!(room.encoded_records(), 0);
    }
}
