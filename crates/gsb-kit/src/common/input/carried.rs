//! What the private frame being written carries of the session's
//! ONE-SHOT state — the pending input ack, the game's session payload,
//! a one-shot private full — so a fan-out drop of that frame's batch can
//! re-arm it (F11; KIT-ARCHITECTURE §10 "F11").
//!
//! The core reports a dropped batch in the same fan-out iteration as
//! the player's `private` call (`GameLogic::on_batch_dropped`), before
//! any other player's, so ONE slot suffices: every private frame starts
//! it ([`InputSeq::frame`]), the writers fill it, and a drop reported
//! for the same player reads it back ([`InputSeq::dropped`]). A drop of
//! a batch whose frame carried none of these re-arms nothing. Nothing
//! here writes bytes: a frame is exactly what it was before, and a
//! re-armed item rides the player's next frame the way it rode this one.

use gsb_core::id::PlayerId;

use super::InputSeq;

/// The one-shot content of the private frame being written.
#[derive(Debug, Default)]
pub(super) struct Carried {
    /// Whose frame (`None`: no frame started since the last drop).
    player: Option<PlayerId>,
    /// The frame carries an ack: the mark reported BEFORE it.
    ack_from: Option<u64>,
    /// The frame carries the game's session payload.
    greeting: bool,
    /// The frame is a one-shot private full.
    full: bool,
}

impl InputSeq {
    /// Start `player`'s private frame for this tick: nothing carried yet.
    pub(crate) fn frame(&mut self, player: PlayerId) {
        self.carried = Carried {
            player: Some(player),
            ..Carried::default()
        };
    }

    /// The frame carries an ack; `from` is the mark reported before it.
    pub(super) fn carries_ack(&mut self, from: u64) {
        self.carried.ack_from = Some(from);
    }

    /// The frame carries the game's session payload.
    pub(crate) fn carries_greeting(&mut self) {
        self.carried.greeting = true;
    }

    /// The frame is a one-shot private full.
    pub(crate) fn carries_full(&mut self) {
        self.carried.full = true;
    }

    /// `player`'s batch was dropped (`GameLogic::on_batch_dropped`): the
    /// ack its frame carried is owed again (the mark goes back to what
    /// was reported before it — the next frame re-sends the high-water
    /// mark, a value the client may already hold, which the ack's
    /// high-water semantics make harmless), and so is the session
    /// payload. Returns whether the frame was a one-shot private full —
    /// the baseline half is the room's ([`super::super::Baselines`]).
    pub(crate) fn dropped(&mut self, player: PlayerId) -> bool {
        let carried = std::mem::take(&mut self.carried);
        if carried.player != Some(player) {
            return false;
        }
        if let Some(st) = self.states.get_mut(&player) {
            if let Some(from) = carried.ack_from {
                st.acked = from;
            }
            if carried.greeting {
                st.greet = true;
            }
        }
        carried.full
    }
}

#[cfg(test)]
mod tests;
