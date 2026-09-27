//! What kind of inbound frame a lost one was, by its opcode alone — the
//! split every count of unprocessed input uses (BACKLOG B60, B58), so an
//! RPC request lost anywhere lands in a term of the RPC ledger
//! (`docs/RPC-CONTROL-PLANE.md` §8.3) and never among the game input.

use gsb_protocol::op;

/// An inbound frame's kind, for counting what was lost unprocessed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameKind {
    /// An RPC request (`RPC_REQ`): owed one answer; a term of the RPC
    /// ledger wherever it is lost.
    Request,
    /// A game-band frame (opcode at or past `GAME_BAND_START`,
    /// registered or not): the game's input.
    Action,
    /// Any other base-band frame: AUTH, JOIN, LEAVE, HEARTBEAT, an
    /// undefined base-band opcode.
    Control,
}

impl FrameKind {
    /// The kind of a frame with opcode `op`.
    pub fn of(op: u16) -> Self {
        if op == op::base::RPC_REQ {
            Self::Request
        } else if op >= op::GAME_BAND_START {
            Self::Action
        } else {
            Self::Control
        }
    }
}
