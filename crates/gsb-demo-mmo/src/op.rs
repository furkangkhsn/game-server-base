//! The MMO's opcodes — game band (`>= gsb_protocol::op::GAME_BAND_START`,
//! the loss-tolerant band: a lost move or snapshot is superseded by the
//! next one).
//!
//! The MMO takes its own block, `1200..`, distinct from the 2D demo's
//! (`1000..=1006`) and the arena's (`1100..=1102`): a server that ever
//! hosts several games registers them in ONE message table, and disjoint
//! numbers keep that possible without a renumbering (an opcode is a
//! contract like a protobuf field number).

/// Client → server: "walk my character toward (x, z)" (`mmo.MoveTo`).
pub const MMO_MOVE_TO: u16 = 1200;

/// Server → client: the AOI group's snapshot (`gsb.kit.WorldSnapshot`;
/// the client decodes it with the typed mirror `mmo.WorldSnapshot`). The
/// MMO's `Game::SNAPSHOT_OP`.
pub const MMO_SNAPSHOT: u16 = 1201;

/// Server → client: the per-connection frame — input ack, one-shot full
/// view, RPC responses (`gsb.kit.Private`; typed mirror `mmo.Private`).
/// The MMO's `Game::PRIVATE_OP`.
pub const MMO_PRIVATE: u16 = 1202;

/// Client → server: "hit this mob" (`mmo.Attack`).
pub const MMO_ATTACK: u16 = 1203;

/// Client → server: "use this waystone" (`mmo.Travel`).
pub const MMO_TRAVEL: u16 = 1204;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opcodes_are_in_the_game_band_distinct_and_in_their_own_block() {
        let ops = [
            MMO_MOVE_TO,
            MMO_SNAPSHOT,
            MMO_PRIVATE,
            MMO_ATTACK,
            MMO_TRAVEL,
        ];
        for (i, op) in ops.into_iter().enumerate() {
            assert!(op >= gsb_protocol::op::GAME_BAND_START, "{op}");
            // Clear of the 2D demo (1000..=1006) and the arena (1100..=1102).
            assert!((1200..1300).contains(&op), "{op}");
            assert!(!ops[..i].contains(&op), "{op} repeated");
        }
    }
}
