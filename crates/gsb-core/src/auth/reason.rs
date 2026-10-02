//! The ticket-refusal vocabulary: every failed AUTH is counted under
//! exactly one [`TicketReason`], and a game's own check under its
//! [`GameReason`] name besides ("her şeyi saymalıyız").

/// Why a ticket was refused — the closed set
/// `gsb_net_tickets_rejected_total{reason}` counts by (zeros included).
///
/// Most reasons are a validator's ([`super::TicketError::Refused`]); four
/// are the engine's own: [`Self::Missing`] (no ticket on a ticket-auth
/// server), [`Self::TimedOut`] (the hook's deadline), [`Self::ValidatorLost`]
/// (the validator's worker died without answering) and [`Self::Other`] (a
/// validator's free-text [`super::TicketError::Rejected`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TicketReason {
    /// A ticket-auth server got an AUTH with no ticket.
    Missing,
    /// Not a token of the validator's format (version, encoding, size,
    /// an undecodable payload).
    Malformed,
    /// The ticket names an issuer key the validator does not trust.
    UnknownKey,
    /// The signature does not verify under the issuer's key.
    Signature,
    /// The signature verified but a required claim is missing or
    /// ill-formed (an empty player, a room that is not a number, the
    /// game's claims not of the game's type).
    Claims,
    /// Past its expiry (beyond the clock-skew allowance).
    Expired,
    /// Issued in the future, or before its not-before (beyond the skew).
    NotYetValid,
    /// Valid for longer than the validator accepts (an issuer minting
    /// long-lived tickets is refused, not trusted).
    Lifetime,
    /// Minted for another audience (another realm or server).
    Audience,
    /// A single-use ticket presented a second time.
    Replayed,
    /// The replay guard could not answer (full, or gone): refused, never
    /// waved through.
    ReplayUnavailable,
    /// The game's own check refused it (see [`GameReason`]).
    Game,
    /// A validator's free-text rejection.
    Other,
    /// The validation did not finish within the hook's timeout.
    TimedOut,
    /// The validator's worker ended without an answer (it panicked).
    ValidatorLost,
}

impl TicketReason {
    /// Every reason, in export order.
    pub const ALL: [TicketReason; 15] = [
        Self::Missing,
        Self::Malformed,
        Self::UnknownKey,
        Self::Signature,
        Self::Claims,
        Self::Expired,
        Self::NotYetValid,
        Self::Lifetime,
        Self::Audience,
        Self::Replayed,
        Self::ReplayUnavailable,
        Self::Game,
        Self::Other,
        Self::TimedOut,
        Self::ValidatorLost,
    ];

    /// The number of reasons.
    pub const COUNT: usize = Self::ALL.len();

    /// This reason's slot in [`Self::ALL`].
    pub const fn index(self) -> usize {
        self as usize
    }

    /// The stable label (the exposition's `reason="…"`, the log line's
    /// `ticket_reject_<label>=`, the client's error text).
    pub const fn label(self) -> &'static str {
        match self {
            Self::Missing => "missing",
            Self::Malformed => "malformed",
            Self::UnknownKey => "unknown_key",
            Self::Signature => "signature",
            Self::Claims => "claims",
            Self::Expired => "expired",
            Self::NotYetValid => "not_yet_valid",
            Self::Lifetime => "lifetime",
            Self::Audience => "audience",
            Self::Replayed => "replayed",
            Self::ReplayUnavailable => "replay_unavailable",
            Self::Game => "game",
            Self::Other => "other",
            Self::TimedOut => "timed_out",
            Self::ValidatorLost => "validator_lost",
        }
    }
}

/// The longest game reason name, in bytes.
pub const GAME_REASON_MAX: usize = 32;

/// The name a game's own ticket check refuses under ("season_pass",
/// "client_too_old", "region") — counted exactly, in
/// `gsb_net_ticket_game_rejects_total{check="<name>"}`.
///
/// Declared once, as a constant: `const SEASON: GameReason =
/// GameReason::new("season_pass");` — a bad name is then a compile
/// error. The name is stored inline, so the value is `Copy` and rides a
/// metrics sample without allocating.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct GameReason {
    name: [u8; GAME_REASON_MAX],
    len: u8,
}

impl GameReason {
    /// A reason named `name`. Panics — at compile time in a `const` item
    /// — unless `name` is 1–32 bytes of `[a-z0-9_]` starting with a
    /// letter.
    pub const fn new(name: &str) -> Self {
        match Self::parse(name) {
            Some(r) => r,
            None => panic!("a game reason is 1-32 bytes of [a-z0-9_] and starts with a letter"),
        }
    }

    /// `None` when `name` is not a valid reason name (see [`Self::new`]).
    pub const fn parse(name: &str) -> Option<Self> {
        let b = name.as_bytes();
        if b.is_empty() || b.len() > GAME_REASON_MAX || !b[0].is_ascii_lowercase() {
            return None;
        }
        let mut out = [0u8; GAME_REASON_MAX];
        let mut i = 0;
        while i < b.len() {
            let c = b[i];
            if !(c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'_') {
                return None;
            }
            out[i] = c;
            i += 1;
        }
        Some(Self {
            name: out,
            len: b.len() as u8,
        })
    }

    /// The reason's name.
    pub fn name(&self) -> &str {
        // Validated ASCII at construction.
        std::str::from_utf8(&self.name[..usize::from(self.len)]).unwrap_or("")
    }
}

impl std::fmt::Debug for GameReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("GameReason").field(&self.name()).finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_reason_sits_at_its_index_with_a_distinct_label() {
        for (i, r) in TicketReason::ALL.iter().enumerate() {
            assert_eq!(r.index(), i, "{r:?}");
        }
        let mut labels: Vec<&str> = TicketReason::ALL.iter().map(|r| r.label()).collect();
        labels.sort_unstable();
        labels.dedup();
        assert_eq!(labels.len(), TicketReason::COUNT);
    }

    #[test]
    fn a_game_reason_takes_only_a_metric_safe_name() {
        const SEASON: GameReason = GameReason::new("season_pass");
        assert_eq!(SEASON.name(), "season_pass");
        for bad in ["", "Season", "1st", "a-b", "a b", &"x".repeat(33)] {
            assert!(GameReason::parse(bad).is_none(), "{bad:?}");
        }
        assert!(GameReason::parse(&"x".repeat(32)).is_some());
    }
}
