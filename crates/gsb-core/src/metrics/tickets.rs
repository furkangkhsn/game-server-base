//! The ticket-auth counters (B21): every AUTH a ticket-auth server
//! decided, accepted or refused — the refusals by [`TicketReason`], the
//! game's own checks by their exact [`GameReason`] name besides.
//!
//! The connection actor counts its own AUTH outcomes and hands them on
//! as a delta in its samples; the collector sums them into
//! [`crate::metrics::NetReport::tickets`]. The ledger is closed:
//! `accepted + rejected_total() = AUTHs a ticket-auth server answered`.

use crate::auth::{GameReason, TicketReason};

/// The most distinct game reason names one set carries; a name beyond
/// it is still counted under `reason="game"` and in
/// [`TicketCounts::game_dropped`], only its name is lost. A game's set
/// of check names is static, so the bound is a design limit met in the
/// first test run.
pub const TICKET_GAME_REASONS_MAX: usize = 8;

const EMPTY: (GameReason, u64) = (GameReason::new("unused"), 0);

/// Ticket-auth outcomes (cumulative in a report, a delta in a sample).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TicketCounts {
    accepted: u64,
    rejected: [u64; TicketReason::COUNT],
    game: [(GameReason, u64); TICKET_GAME_REASONS_MAX],
    game_len: u8,
    game_dropped: u64,
}

impl Default for TicketCounts {
    fn default() -> Self {
        Self {
            accepted: 0,
            rejected: [0; TicketReason::COUNT],
            game: [EMPTY; TICKET_GAME_REASONS_MAX],
            game_len: 0,
            game_dropped: 0,
        }
    }
}

impl TicketCounts {
    /// Count an accepted ticket.
    pub fn accept(&mut self) {
        self.accepted = self.accepted.saturating_add(1);
    }

    /// Count a refusal for `reason`.
    pub fn reject(&mut self, reason: TicketReason) {
        let slot = &mut self.rejected[reason.index()];
        *slot = slot.saturating_add(1);
    }

    /// Count a refusal by the game's own check `name` (under
    /// [`TicketReason::Game`] too).
    pub fn reject_game(&mut self, name: GameReason) {
        self.reject(TicketReason::Game);
        self.add_game(name, 1);
    }

    fn add_game(&mut self, name: GameReason, n: u64) {
        let len = usize::from(self.game_len);
        if let Some(slot) = self.game[..len].iter_mut().find(|(g, _)| *g == name) {
            slot.1 = slot.1.saturating_add(n);
        } else if len < TICKET_GAME_REASONS_MAX {
            self.game[len] = (name, n);
            self.game_len += 1;
        } else {
            self.game_dropped = self.game_dropped.saturating_add(n);
        }
    }

    /// Add another set (the collector's fold).
    pub fn add_all(&mut self, o: &Self) {
        self.accepted = self.accepted.saturating_add(o.accepted);
        for (a, b) in self.rejected.iter_mut().zip(o.rejected) {
            *a = a.saturating_add(b);
        }
        for (name, n) in o.game_slots() {
            self.add_game(*name, *n);
        }
        self.game_dropped = self.game_dropped.saturating_add(o.game_dropped);
    }

    /// Accepted tickets.
    pub fn accepted(&self) -> u64 {
        self.accepted
    }

    /// Refusals for one reason.
    pub fn rejected(&self, reason: TicketReason) -> u64 {
        self.rejected[reason.index()]
    }

    /// Every refusal, summed over the reasons.
    pub fn rejected_total(&self) -> u64 {
        self.rejected.iter().fold(0u64, |a, n| a.saturating_add(*n))
    }

    /// `(reason, count)` for every reason, in export order.
    pub fn reasons(&self) -> impl Iterator<Item = (TicketReason, u64)> + '_ {
        TicketReason::ALL.iter().map(|r| (*r, self.rejected(*r)))
    }

    /// The game's named refusals, in the order the names first appeared.
    pub fn game_slots(&self) -> &[(GameReason, u64)] {
        &self.game[..usize::from(self.game_len)]
    }

    /// Game refusals whose name did not fit the bound (counted, unnamed).
    pub fn game_dropped(&self) -> u64 {
        self.game_dropped
    }

    /// Nothing counted.
    pub fn is_empty(&self) -> bool {
        self.accepted == 0 && self.rejected_total() == 0 && self.game_dropped == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_game_names_are_counted_apart_and_the_overflow_is_kept() {
        let mut t = TicketCounts::default();
        t.accept();
        t.reject(TicketReason::Expired);
        let names: Vec<String> = (0..=TICKET_GAME_REASONS_MAX)
            .map(|i| format!("check_{i}"))
            .collect();
        for n in &names {
            t.reject_game(GameReason::parse(n).expect("valid"));
        }
        t.reject_game(GameReason::new("check_0"));
        assert_eq!(t.accepted(), 1);
        assert_eq!(t.rejected(TicketReason::Expired), 1);
        assert_eq!(t.rejected(TicketReason::Game), names.len() as u64 + 1);
        assert_eq!(t.game_slots()[0].1, 2, "the same name folds");
        assert_eq!(t.game_slots().len(), TICKET_GAME_REASONS_MAX);
        assert_eq!(t.game_dropped(), 1, "the ninth name is counted unnamed");

        let mut sum = TicketCounts::default();
        sum.add_all(&t);
        sum.add_all(&t);
        assert_eq!(sum.rejected_total(), 2 * t.rejected_total());
        assert_eq!(sum.game_slots()[0].1, 4);
        assert_eq!(sum.game_dropped(), 2);
    }
}
