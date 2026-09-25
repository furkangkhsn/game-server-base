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
use bytes::BytesMut;
use gsb_core::id::PlayerId;
use gsb_core::rpc::RpcReply;

use crate::common::{InputSeq, emit_private, put_delimited};
use crate::game::Game;

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
