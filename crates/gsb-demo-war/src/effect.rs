//! The war game's remote-effect payload — the game-owned bytes inside a
//! core `RemoteEffect` (`docs/CROSS-SHARD.md` §2): a blow on an enemy a
//! neighbouring shard owns. Never on the client wire: it travels between
//! shards only, so it is a fixed three-byte layout rather than a message
//! of `war.proto` (the client contract).

use bytes::Bytes;

/// What one effect does to its target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WarEffect {
    /// A melee hit of `damage` hit points (the authority caps it at
    /// [`crate::world::ATTACK_DAMAGE`] — it never trusts the payload).
    Strike { damage: u16 },
}

/// The tag byte of [`WarEffect::Strike`].
const STRIKE: u8 = 1;

impl WarEffect {
    /// The payload bytes: `[tag, damage_lo, damage_hi]`.
    #[must_use]
    pub fn encode(self) -> Bytes {
        match self {
            WarEffect::Strike { damage } => {
                let [lo, hi] = damage.to_le_bytes();
                Bytes::copy_from_slice(&[STRIKE, lo, hi])
            }
        }
    }

    /// Read a payload back; `None` for anything this game did not write.
    #[must_use]
    pub fn decode(payload: &[u8]) -> Option<Self> {
        match payload {
            [STRIKE, lo, hi] => Some(WarEffect::Strike {
                damage: u16::from_le_bytes([*lo, *hi]),
            }),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_strike_round_trips_and_nothing_else_decodes() {
        let s = WarEffect::Strike { damage: 25 };
        assert_eq!(WarEffect::decode(&s.encode()), Some(s));
        assert_eq!(WarEffect::decode(&[]), None);
        assert_eq!(WarEffect::decode(&[2, 25, 0]), None);
        assert_eq!(WarEffect::decode(&[1, 25]), None);
    }
}
