//! The logic's own counters (F9) on the RESULT line: one generic
//! `logic_<name>=<value>` key per counter the hosted game's room
//! reports, from the run's last folded report (cumulative, each counter
//! folded across the shards by its own rule — see `fold.rs`).
//!
//! Generic, not a per-game segment: the load generator does not know
//! what a game counts, and a counter the game adds tomorrow reaches the
//! line without a change here. A room that declares nothing adds
//! nothing — the line is exactly what it was before the seam.

use gsb_core::metrics::LogicCounters;

/// ` logic_<name>=<value>` per counter, in the order the logic put
/// them, then ` logic_counters_dropped=<n>` while the bound dropped any
/// (F17, the server line's key); empty without a report or without
/// counters.
pub(crate) fn logic_segment(total: Option<&LogicCounters>) -> String {
    total
        .map(|logic| {
            let mut seg: String = logic
                .slots()
                .iter()
                .map(|s| format!(" logic_{}={}", s.counter.name(), s.value))
                .collect();
            if logic.dropped() > 0 {
                seg.push_str(&format!(" logic_counters_dropped={}", logic.dropped()));
            }
            seg
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use gsb_core::metrics::LogicCounter;

    #[test]
    fn each_counter_is_one_key_in_put_order() {
        let mut set = LogicCounters::new();
        set.put(&LogicCounter::sum("crystal_moves", ""), 12);
        set.put(&LogicCounter::max("crystal_fights_peak", ""), 3);
        assert_eq!(
            logic_segment(Some(&set)),
            " logic_crystal_moves=12 logic_crystal_fights_peak=3"
        );
    }

    /// The bound's overflow follows the counters, only while non-zero.
    #[test]
    fn an_overflow_is_one_more_key_after_the_counters() {
        let mut set = LogicCounters::new();
        set.put(&LogicCounter::sum("kills", ""), 4);
        set.add_dropped(2);
        assert_eq!(
            logic_segment(Some(&set)),
            " logic_kills=4 logic_counters_dropped=2"
        );
    }

    #[test]
    fn no_counters_or_no_report_adds_nothing() {
        assert_eq!(logic_segment(Some(&LogicCounters::new())), "");
        assert_eq!(logic_segment(None), "");
    }
}
