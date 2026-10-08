//! The room's input record (docs/EVENTS.md §11): the engine's
//! `determinism::Input` grows one arm — `Events`, what the EVENTS phase
//! drained, in order — filling the `#[non_exhaustive]` slot B178 left.
//! A replay hands the same list to the same EVENTS code (owner
//! resolution, `no_handler`, decode), so the table holds the same events
//! in the same order and the hashes match.

use crate::contract::{ConnId, Identity, Owner};
use crate::room::{Action, Completion, RoomEventMsg};

/// A CONTROL input (receivers stripped: a replay has no channels).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ControlRec {
    /// A join.
    Join { identity: Identity, conn: ConnId, owner: Owner, carried: u64 },
    /// A leave.
    Leave { identity: Identity },
    /// A park (the transport died; the entity stays).
    Park { identity: Identity },
    /// A resume on a new session.
    Resume { identity: Identity, conn: ConnId },
}

/// One input, in the order the room processed it.
#[derive(Debug, Clone)]
pub enum Input {
    /// CONTROL.
    Control(ControlRec),
    /// RESPONSES.
    Responses(Vec<Completion>),
    /// EVENTS (NEW — file tag 18 in the engine's format; additive).
    Events(Vec<RoomEventMsg>),
    /// READ.
    Read(Vec<(Identity, Action)>),
    /// SYSTEMS.
    Systems,
    /// The tick's end.
    EndTick,
}

impl Input {
    /// The phase name (the order proof).
    pub fn phase(&self) -> &'static str {
        match self {
            Self::Control(_) => "CONTROL",
            Self::Responses(_) => "RESPONSES",
            Self::Events(_) => "EVENTS",
            Self::Read(_) => "READ",
            Self::Systems => "SYSTEMS",
            Self::EndTick => "END",
        }
    }
}

/// One tick of the record.
#[derive(Debug, Clone)]
pub struct TickRecord {
    /// The room's step number.
    pub tick: u64,
    /// What entered the logic, in order.
    pub inputs: Vec<Input>,
    /// The step's state hash.
    pub hash: u64,
}

/// FNV-1a 64 (the engine's `StateHasher`).
#[derive(Debug, Clone, Copy)]
pub struct Fnv(pub u64);

impl Default for Fnv {
    fn default() -> Self {
        Self(0xcbf2_9ce4_8422_2325)
    }
}

impl Fnv {
    /// Fold bytes.
    pub fn bytes(&mut self, b: &[u8]) {
        for x in b {
            self.0 ^= u64::from(*x);
            self.0 = self.0.wrapping_mul(0x0100_0000_01b3);
        }
    }
    /// Fold a `u64`.
    pub fn u64(&mut self, v: u64) {
        self.bytes(&v.to_le_bytes());
    }
}
