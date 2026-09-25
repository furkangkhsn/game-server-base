//! The MMO's remote-effect payload — the game-owned bytes inside a core
//! `RemoteEffect` (`docs/CROSS-SHARD.md` §2). Never on the client wire:
//! it travels between shards only, so it is a fixed three-byte layout
//! rather than a message of `mmo.proto` (the client contract).

use bytes::Bytes;

/// What one effect does to its target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MmoEffect {
    /// A melee hit of `damage` hit points (the authority caps it at
    /// [`crate::world::ATTACK_DAMAGE`] — it never trusts the payload).
    Strike { damage: u16 },
}

/// The tag byte of [`MmoEffect::Strike`].
const STRIKE: u8 = 1;

impl MmoEffect {
    /// The payload bytes: `[tag, damage_lo, damage_hi]`.
    #[must_use]
    pub fn encode(self) -> Bytes {
        match self {
            MmoEffect::Strike { damage } => {
                let [lo, hi] = damage.to_le_bytes();
                Bytes::copy_from_slice(&[STRIKE, lo, hi])
            }
        }
    }

    /// Read a payload back; `None` for anything this game did not write.
    #[must_use]
    pub fn decode(payload: &[u8]) -> Option<Self> {
        match payload {
            [STRIKE, lo, hi] => Some(MmoEffect::Strike {
                damage: u16::from_le_bytes([*lo, *hi]),
            }),
            _ => None,
        }
    }
}
