//! The game's session payload in the `Private` frame (`Private.game`,
//! field 4 — KIT-ARCHITECTURE §5.1): what [`Game::session_private`]
//! writes, owed once per session (a join, a resume — [`InputSeq`]'s
//! greeting) and appended to the frame the room ships anyway.
//!
//! Field 4 is the frame's last field, so appending it after the
//! ack/responses frame (generated encoder) or after the hand-encoded
//! one-shot full gives the bytes a generated encoder of the whole frame
//! would write. A game that keeps the default writes nothing: the frame,
//! and whether there is one, are exactly as without the hook.

use std::collections::HashMap;

use bevy_ecs::prelude::{Entity, World};
use bytes::{BufMut, BytesMut};
use gsb_core::id::PlayerId;
use gsb_core::rpc::RpcReply;
use prost::encoding::varint::encode_varint;

use crate::common::{InputSeq, append_responses, emit_private, put_delimited};
use crate::game::Game;

/// `snapshot` (field 2 of the `payload` oneof), length-delimited.
const TAG_SNAPSHOT: u8 = 0x12;
/// `game` (field 4), length-delimited.
const TAG_GAME: u8 = 0x22;

/// The ordinary private frame ([`emit_private`]: the pending ack and the
/// queued RPC answers) plus, on the session's first frame, the game's
/// session payload. Returns `true` when a frame was produced.
pub(crate) fn emit_private_frame<G: Game>(
    game: &mut G,
    world: &World,
    players: &HashMap<PlayerId, Entity>,
    input: &mut InputSeq,
    player: PlayerId,
    responses: &[RpcReply],
    out: &mut BytesMut,
) -> bool {
    let framed = emit_private(input, player, responses, out);
    let greeted = append_session_payload(game, world, players, input, player, out);
    framed || greeted
}

/// The one-shot private FULL frame — a connection's baseline reset
/// (`kit.proto`, `Private.snapshot`): the pre-encoded FULL
/// `WorldSnapshot` `full` in the `snapshot` oneof arm (field 2,
/// length-delimited — the kit hand-encodes the tag), this tick's RPC
/// answers on the SAME frame (field 3 — the per-connection per-tick slot
/// is one frame) and, on the session's first frame, the game's session
/// payload (field 4, last). Always produces a frame. The pending input
/// ack is not written: it rides the connection's next ordinary frame.
///
/// Shared by every delta room (the AOI rooms, the team room in delta
/// mode): which connection is owed one is each room's session surface;
/// the frame is one.
#[allow(clippy::too_many_arguments)] // the tables, the player, the frame parts
pub(crate) fn emit_private_full<G: Game>(
    game: &mut G,
    world: &World,
    players: &HashMap<PlayerId, Entity>,
    input: &mut InputSeq,
    player: PlayerId,
    full: &[u8],
    responses: &[RpcReply],
    out: &mut BytesMut,
) -> bool {
    out.put_u8(TAG_SNAPSHOT);
    encode_varint(full.len() as u64, out);
    out.extend_from_slice(full);
    append_responses(responses, out);
    append_session_payload(game, world, players, input, player, out);
    true
}

/// Append `player`'s session payload to the frame in `out` when it is
/// owed (consuming the greeting) and the game has one. Returns `true`
/// when field 4 was written.
pub(crate) fn append_session_payload<G: Game>(
    game: &mut G,
    world: &World,
    players: &HashMap<PlayerId, Entity>,
    input: &mut InputSeq,
    player: PlayerId,
    out: &mut BytesMut,
) -> bool {
    if !input.take_greeting(player) {
        return false;
    }
    let Some(&entity) = players.get(&player) else {
        return false;
    };
    let at = out.len();
    let mut wrote = false;
    put_delimited(out, TAG_GAME, |o| {
        wrote = game.session_private(world, entity, o);
    });
    if !wrote {
        out.truncate(at);
    }
    wrote
}

#[cfg(test)]
mod tests;
