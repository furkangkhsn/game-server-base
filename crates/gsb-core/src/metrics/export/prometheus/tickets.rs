//! The ticket-auth families (B21): the accepted count, the refusals by
//! `reason` (the closed set, zeros included), the game's own checks by
//! `check` name (present once counted) and the names past the bound
//! (present while non-zero).

use std::fmt::Write as _;

use super::header;
use crate::metrics::TicketCounts;
use crate::metrics::export::families::{
    TICKET_GAME_NAMES_DROPPED, TICKET_GAME_REJECTS, TICKETS_ACCEPTED, TICKETS_REJECTED,
};

pub(super) fn render(out: &mut String, t: &TicketCounts) {
    let (name, help) = TICKETS_ACCEPTED;
    header(out, name, "counter", help);
    let _ = writeln!(out, "{name} {}", t.accepted());
    let (name, help) = TICKETS_REJECTED;
    header(out, name, "counter", help);
    for (reason, n) in t.reasons() {
        let _ = writeln!(out, "{name}{{reason=\"{}\"}} {n}", reason.label());
    }
    if !t.game_slots().is_empty() {
        let (name, help) = TICKET_GAME_REJECTS;
        header(out, name, "counter", help);
        for (check, n) in t.game_slots() {
            let _ = writeln!(out, "{name}{{check=\"{}\"}} {n}", check.name());
        }
    }
    if t.game_dropped() > 0 {
        let (name, help) = TICKET_GAME_NAMES_DROPPED;
        header(out, name, "counter", help);
        let _ = writeln!(out, "{name} {}", t.game_dropped());
    }
}
