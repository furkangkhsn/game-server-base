//! The demo AI handover: a disconnected player's entity keeps moving
//! under synthesized input until it is reclaimed.

use bevy_ecs::prelude::{Entity, World};
use gsb_core::id::{ConnectionId, PlayerId};
use gsb_core::room::{Action, TickCtx};
use prost::Message;

use crate::components::Position;
use crate::op;

/// How often the demo bot picks a new wander target, in ticks (~1 s at
/// the default 30 Hz).
const BOT_WANDER_EVERY_TICKS: u64 = 30;

/// How far from its current position the bot may wander (world units) —
/// small enough that a reclaimed hero is roughly where its team left it.
const BOT_WANDER_RADIUS: f32 = 12.0;

/// Deterministic wander jitter for one bot round: an integer hash over
/// (position bits, round index) mapped into
/// `[-BOT_WANDER_RADIUS, +BOT_WANDER_RADIUS]²`. No RNG dependency, stable
/// for a given (where it stands, when asked) pair — the spec's "derive
/// from entity position + tick counter".
fn bot_jitter(x_bits: u32, y_bits: u32, round: u64) -> (f32, f32) {
    let mut z = (x_bits as u64)
        ^ (y_bits as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15)
        ^ round.wrapping_mul(0xD1B5_4A32_D192_ED03);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^= z >> 31;
    let half = BOT_WANDER_RADIUS;
    let x = ((z & 0xFFFF) as f32 / 65535.0 - 0.5) * 2.0 * half;
    let y = ((z >> 16 & 0xFFFF) as f32 / 65535.0 - 0.5) * 2.0 * half;
    (x, y)
}

/// The demo bot (RECONNECT §9): synthesize MOVE_TO frames for every
/// bot-fed entity — handed in as `(its dead connection id, the entity)`
/// pairs by each room's `ingest` (the single-world rooms read them off
/// their ledger entries; the sharded room resolves its wire-keyed ledger
/// through its own tables first) — and push them INTO the tick's action
/// list, ahead of any wire actions. They are indistinguishable from
/// client frames: the ordinary [`ingest`] decodes them, applies the
/// sequence rule (seq = 0: unnumbered, never fights a human high-water
/// mark) and writes the real [`MoveTarget`] — which is the whole point:
/// the bot is an input source without a connection, exercising the REAL
/// movement system, not a parallel teleport path.
pub(crate) fn synthesize_bot_moves(
    bots: impl Iterator<Item = (PlayerId, Entity)>,
    world: &World,
    ctx: &TickCtx,
    actions: &mut Vec<Action>,
) {
    // One cadence gate for the whole tick: the bot acts on every Nth tick
    // only (the cheap common case short-circuits here).
    if !ctx.tick.is_multiple_of(BOT_WANDER_EVERY_TICKS) {
        return;
    }
    let round = ctx.tick / BOT_WANDER_EVERY_TICKS;
    for (player, entity) in bots {
        let Ok(he) = world.get_entity(entity) else {
            continue;
        };
        let Some(pos) = he.get::<Position>().copied() else {
            continue;
        };
        let (jx, jy) = bot_jitter(pos.x.to_bits(), pos.y.to_bits(), round);
        let msg = crate::game::MoveTo {
            x: (pos.x + jx) as i32,
            y: (pos.y + jy) as i32,
            seq: 0,
        };
        actions.push(Action {
            // Synthesized input has no transport session behind it; the
            // stable player id is what routes it.
            conn: ConnectionId(0),
            player,
            op: op::MOVE_TO,
            payload: msg.encode_to_vec().into(),
        });
    }
}

// ════════════════════════════════════════════════════════════════════════
// The shared CELL-DELTA machinery: the spatial visibility strategies run
// the same encoding engine, so it lives here once. Two rooms drive it —
// [`crate::aoi::AoiRoom`] (single world) and
// [`crate::sharded::ShardedSpatialRoom`] (the Faz B per-shard composite) —
// and they differ only in WHAT feeds the bookkeeping (bevy's dirty query
// alone vs the dirty query PLUS a diff of the borrowed border strip) and
// in who counts as a member. The wire format (header/pieces/oneof framing)
// and the delta arithmetic
// (change list = the diff, order-independent flags/birth roll) are
// byte-for-byte common.
//
// What deliberately stayed per-room: the session surface (`conn_view`,
// `group_full_emitted` consumers, `private`'s one-shot shape) and — on the
// sharded side — the borrowed-strip ledger, which is that room's
// load-bearing subtlety (see its module docs).
// ════════════════════════════════════════════════════════════════════════
