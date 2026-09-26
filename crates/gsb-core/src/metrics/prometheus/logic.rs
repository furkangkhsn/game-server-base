//! The logic's own counters (F9) in the exposition: one family per
//! counter NAME, the room as its label — the same shape as every core
//! room family.
//!
//! Per-name families, not one family with a `name` label
//! (`gsb_room_logic_total{name="kills"}`): each counter keeps its own
//! HELP line and its own type (a SUM is a `counter`, a MAX high-water
//! mark a `gauge` — one family cannot be both), and a query reads like
//! any core counter (`rate(gsb_room_logic_kills_total[1m])`). The
//! cardinality is the same either way: names are static per game, and
//! the per-room label is what every room family already carries.

use std::fmt::Write as _;

use crate::metrics::{LogicCounter, LogicFold, RoomReport};

/// One family: its metric name, the counter that named it (help,
/// rule), and its `(room id, value)` samples.
type Family = (String, LogicCounter, Vec<(u64, u64)>);

/// The families in the order their names first appear (room order, then
/// the order each logic put them), each with its samples, so a family's
/// lines stay together however many rooms report it. A room that does
/// not report a name has no line in its family.
pub(super) fn render(out: &mut String, rooms: &[RoomReport]) {
    let mut families: Vec<Family> = Vec::new();
    for r in rooms {
        for s in r.logic.slots() {
            let name = family_name(&s.counter);
            match families.iter_mut().find(|f| f.0 == name) {
                Some(f) => f.2.push((r.room.0, s.value)),
                None => families.push((name, s.counter, vec![(r.room.0, s.value)])),
            }
        }
    }
    for (name, counter, samples) in families {
        let (kind, help) = match counter.fold() {
            LogicFold::Sum => ("counter", "The logic's own counter, cumulative."),
            LogicFold::Max => ("gauge", "The logic's own high-water mark."),
        };
        let help = if counter.help().is_empty() {
            help
        } else {
            counter.help()
        };
        let _ = write!(out, "# HELP {name} {help}\n# TYPE {name} {kind}\n");
        for (room, v) in samples {
            let _ = writeln!(out, "{name}{{room=\"r{room}\"}} {v}");
        }
    }
}

/// `gsb_room_logic_<name>_total` for a SUM, `gsb_room_logic_<name>` for
/// a MAX (a gauge carries no `_total`). The name's own rules (no
/// `_total` suffix) keep the two forms from colliding.
fn family_name(c: &LogicCounter) -> String {
    match c.fold() {
        LogicFold::Sum => format!("gsb_room_logic_{}_total", c.name()),
        LogicFold::Max => format!("gsb_room_logic_{}", c.name()),
    }
}
