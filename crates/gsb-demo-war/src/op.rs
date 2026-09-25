//! The war game's opcodes — game band (`>= gsb_protocol::op::GAME_BAND_START`,
//! the loss-tolerant band: a lost move or snapshot is superseded by the
//! next one).
//!
//! Its own block, `1300..`, clear of the 2D demo (`1000..=1006`), the
//! arena (`1100..=1102`) and the MMO (`1200..=1204`): a server that ever
//! hosts several games registers them in ONE message table, and disjoint
//! numbers keep that possible without a renumbering.

/// Client → server: "walk my unit toward (x, z)" (`war.MoveTo`).
pub const WAR_MOVE_TO: u16 = 1300;

/// Server → client: the faction's snapshot (`gsb.kit.WorldSnapshot`;
/// typed mirror `war.WorldSnapshot`). The game's `Game::SNAPSHOT_OP`.
pub const WAR_SNAPSHOT: u16 = 1301;

/// Server → client: the per-connection frame — input ack, one-shot full
/// view, RPC responses, the session's `Welcome` (`gsb.kit.Private`;
/// typed mirror `war.Private`). The game's `Game::PRIVATE_OP`.
pub const WAR_PRIVATE: u16 = 1302;

/// Client → server: "hit this enemy" (`war.Attack`).
pub const WAR_ATTACK: u16 = 1303;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opcodes_are_in_the_game_band_distinct_and_in_their_own_block() {
        let ops = [WAR_MOVE_TO, WAR_SNAPSHOT, WAR_PRIVATE, WAR_ATTACK];
        for (i, op) in ops.into_iter().enumerate() {
            assert!(op >= gsb_protocol::op::GAME_BAND_START, "{op}");
            // Clear of the demo (1000..), the arena (1100..), the MMO (1200..).
            assert!((1300..1400).contains(&op), "{op}");
            assert!(!ops[..i].contains(&op), "{op} repeated");
        }
    }
}
