//! The arena's opcodes — game band (`>= gsb_protocol::op::GAME_BAND_START`,
//! the loss-tolerant band: a lost move or snapshot is superseded by the
//! next one).
//!
//! The arena takes its own block, `1100..`, instead of reusing the 2D
//! demo's numbers (`1000..=1006`): the two are different games with
//! different messages under the same kind of frame, and a server that
//! ever hosts both registers them in ONE message table — disjoint
//! numbers keep that possible without a renumbering (an opcode is a
//! contract like a protobuf field number).

/// Client → server: "move my unit toward (x, y, z)" (`arena.MoveTo`).
pub const ARENA_MOVE_TO: u16 = 1100;

/// Server → client: the team's snapshot (`gsb.kit.WorldSnapshot`; the
/// client decodes it with the typed mirror `arena.WorldSnapshot`). The
/// arena's `Game::SNAPSHOT_OP`.
pub const ARENA_SNAPSHOT: u16 = 1101;

/// Server → client: the per-connection frame — input ack and RPC
/// responses (`gsb.kit.Private`; typed mirror `arena.Private`). The
/// arena's `Game::PRIVATE_OP`.
pub const ARENA_PRIVATE: u16 = 1102;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opcodes_are_in_the_game_band_and_distinct() {
        let ops = [ARENA_MOVE_TO, ARENA_SNAPSHOT, ARENA_PRIVATE];
        for op in ops {
            assert!(op >= gsb_protocol::op::GAME_BAND_START, "{op}");
        }
        assert!(ops[0] != ops[1] && ops[1] != ops[2] && ops[0] != ops[2]);
    }
}
