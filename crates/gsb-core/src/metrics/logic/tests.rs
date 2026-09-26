//! The logic-counter set's rules: the declaration check, the bound and
//! its overflow count, and the fold of a name met twice.

use super::*;

const KILLS: LogicCounter = LogicCounter::sum("kills", "Players felled.");
const PEAK: LogicCounter = LogicCounter::max("fights_peak", "Largest fight table.");

/// The i-th distinct valid name (the declaration copies the name, so
/// a run-time string will do).
fn nth(i: usize) -> LogicCounter {
    LogicCounter::sum(&format!("c{i}"), "")
}

#[test]
fn a_declared_counter_keeps_its_name_help_and_rule() {
    assert_eq!(KILLS.name(), "kills");
    assert_eq!(KILLS.help(), "Players felled.");
    assert_eq!(KILLS.fold(), LogicFold::Sum);
    assert_eq!(PEAK.fold(), LogicFold::Max);
    let longest = "a".repeat(LOGIC_NAME_MAX);
    assert_eq!(LogicCounter::sum(&longest, "").name(), longest);
}

/// Names are what both exposures can carry as they are: a Prometheus
/// metric-name fragment and a `key=value` key.
#[test]
fn only_lowercase_snake_names_that_fit_are_valid() {
    for bad in [
        "",
        "Kills",
        "kiLLs",
        "9lives",
        "_kills",
        "kills total",
        "kills=1",
        "kills-2",
        "kills_total",
        "counters_dropped",
        "caf\u{e9}",
    ] {
        assert!(
            LogicCounter::parse(bad, "", LogicFold::Sum).is_none(),
            "{bad:?} must be refused"
        );
    }
    let too_long = "a".repeat(LOGIC_NAME_MAX + 1);
    assert!(LogicCounter::parse(&too_long, "", LogicFold::Sum).is_none());
    for good in [
        "k",
        "kills",
        "crystal_moves",
        "war_kills_2",
        "totals",
        "counters_dropped_2",
    ] {
        assert!(
            LogicCounter::parse(good, "", LogicFold::Sum).is_some(),
            "{good}"
        );
    }
}

#[test]
#[should_panic(expected = "a logic counter's name")]
fn declaring_a_bad_name_panics() {
    let _ = LogicCounter::sum("Bad Name", "");
}

#[test]
#[should_panic(expected = "a logic counter's help")]
fn declaring_a_multiline_help_panics() {
    let _ = LogicCounter::sum("ok", "two\nlines");
}

#[test]
fn an_empty_set_is_empty() {
    let set = LogicCounters::new();
    assert!(set.is_empty());
    assert_eq!(set.slots(), &[]);
    assert_eq!(set.dropped(), 0);
    assert_eq!(set, LogicCounters::default());
}

#[test]
fn put_keeps_first_put_order_and_values() {
    let mut set = LogicCounters::new();
    set.put(&KILLS, 3);
    set.put(&PEAK, 9);
    let got: Vec<(&str, u64)> = set
        .slots()
        .iter()
        .map(|s| (s.counter.name(), s.value))
        .collect();
    assert_eq!(got, [("kills", 3), ("fights_peak", 9)]);
    assert_eq!(set.get("kills"), Some(3));
    assert_eq!(set.get("deaths"), None);
}

/// The bound: sixteen names fit, the seventeenth is dropped and
/// counted — and a name already in a full set still folds.
#[test]
fn a_seventeenth_name_is_dropped_and_counted() {
    let mut set = LogicCounters::new();
    for i in 0..LOGIC_COUNTERS_MAX {
        set.put(&nth(i), i as u64);
    }
    assert_eq!(set.slots().len(), LOGIC_COUNTERS_MAX);
    assert_eq!(set.dropped(), 0);
    set.put(&nth(LOGIC_COUNTERS_MAX), 1);
    set.put(&nth(LOGIC_COUNTERS_MAX + 1), 1);
    assert_eq!(set.slots().len(), LOGIC_COUNTERS_MAX);
    assert_eq!(set.dropped(), 2);
    assert_eq!(set.get(&format!("c{LOGIC_COUNTERS_MAX}")), None);
    set.put(&nth(0), 5);
    assert_eq!(set.get("c0"), Some(5), "a known name folds in a full set");
    assert_eq!(set.dropped(), 2);
}

#[test]
fn a_name_put_twice_folds_by_its_rule() {
    let mut set = LogicCounters::new();
    set.put(&KILLS, 3);
    set.put(&KILLS, 4);
    set.put(&PEAK, 9);
    set.put(&PEAK, 2);
    assert_eq!(set.get("kills"), Some(7), "SUM adds");
    assert_eq!(set.get("fights_peak"), Some(9), "MAX keeps the larger");
    assert_eq!(set.slots().len(), 2);
}

/// `merge` is the shard fold: name by name, each by its rule, a name
/// only one side has kept, and the drops added.
#[test]
fn merge_folds_two_shards_name_by_name() {
    let only_b = LogicCounter::sum("releases", "");
    let mut a = LogicCounters::new();
    a.put(&KILLS, 3);
    a.put(&PEAK, 9);
    a.add_dropped(1);
    let mut b = LogicCounters::new();
    b.put(&PEAK, 12);
    b.put(&KILLS, 4);
    b.put(&only_b, 2);
    b.add_dropped(2);
    a.merge(&b);
    assert_eq!(a.get("kills"), Some(7));
    assert_eq!(a.get("fights_peak"), Some(12));
    assert_eq!(a.get("releases"), Some(2));
    assert_eq!(a.dropped(), 3);
}
