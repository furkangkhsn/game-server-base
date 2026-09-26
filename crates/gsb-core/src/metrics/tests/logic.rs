//! The logic's own counters (F9) from sample to report to both
//! renderings: copied as the latest sample carries them, one
//! `logic_<name>=` key each on the line, one family per name in the
//! exposition (a SUM a `counter`, a MAX a `gauge`), and nothing at all
//! for a room whose logic declares none.

use super::*;

const KILLS: LogicCounter = LogicCounter::sum("kills", "Players felled, cumulative.");
const PEAK: LogicCounter = LogicCounter::max("fights_peak", "Largest fight table seen.");
const CAPTURES: LogicCounter = LogicCounter::sum("captures", "");

fn sample_with(room: u64, t: Instant, counters: &[(LogicCounter, u64)]) -> RoomSample {
    let mut s = room_sample(RoomId(room), t, 10);
    for (c, v) in counters {
        s.logic.put(c, *v);
    }
    s
}

/// Two rooms that report, one that does not: the report of each is its
/// latest sample's set, unchanged.
fn report() -> MetricReport {
    let mut acc = MetricAccumulator::default();
    let t = Instant::now();
    acc.apply(MetricsEvent::Room(sample_with(1, t, &[(KILLS, 1)])));
    acc.apply(MetricsEvent::Room(sample_with(
        1,
        t,
        &[(KILLS, 4), (PEAK, 7)],
    )));
    acc.apply(MetricsEvent::Room(sample_with(2, t, &[])));
    acc.apply(MetricsEvent::Room(sample_with(
        3,
        t,
        &[(PEAK, 2), (CAPTURES, 5), (KILLS, 9)],
    )));
    acc.report(t)
}

#[test]
fn the_report_carries_the_latest_samples_counters() {
    let r = report();
    assert_eq!(r.rooms[0].logic.get("kills"), Some(4), "the latest sample");
    assert_eq!(r.rooms[0].logic.get("fights_peak"), Some(7));
    assert!(r.rooms[1].logic.is_empty());
    assert_eq!(r.rooms[2].logic.slots().len(), 3);
}

#[test]
fn each_counter_is_one_logic_key_after_the_core_keys() {
    let lines = report().render();
    let room = |id: &str| {
        lines
            .iter()
            .find(|l| l.starts_with(&format!("gsb-metric scope=room id={id} ")))
            .expect("the room's line")
            .clone()
    };
    assert!(
        room("r1").ends_with(" metrics_dropped=0 logic_kills=4 logic_fights_peak=7"),
        "{}",
        room("r1")
    );
    assert!(
        room("r2").ends_with(" metrics_dropped=0"),
        "no counters, no keys"
    );
    assert!(room("r3").ends_with(" logic_fights_peak=2 logic_captures=5 logic_kills=9"));
}

#[test]
fn each_name_is_one_family_with_its_help_and_type() {
    let out = report().render_prometheus();
    let block = |family: &str| -> Vec<String> {
        let lines: Vec<&str> = out.lines().collect();
        let at = lines
            .iter()
            .position(|l| l.starts_with(&format!("# HELP {family} ")))
            .unwrap_or_else(|| panic!("no {family} family: {out}"));
        lines[at..]
            .iter()
            .take_while(|l| !l.starts_with("# HELP ") || l.contains(family))
            .map(|l| l.to_string())
            .collect()
    };
    assert_eq!(
        block("gsb_room_logic_kills_total"),
        [
            "# HELP gsb_room_logic_kills_total Players felled, cumulative.",
            "# TYPE gsb_room_logic_kills_total counter",
            "gsb_room_logic_kills_total{room=\"r1\"} 4",
            "gsb_room_logic_kills_total{room=\"r3\"} 9",
        ],
        "the two rooms' lines together, under one header"
    );
    assert_eq!(
        block("gsb_room_logic_fights_peak"),
        [
            "# HELP gsb_room_logic_fights_peak Largest fight table seen.",
            "# TYPE gsb_room_logic_fights_peak gauge",
            "gsb_room_logic_fights_peak{room=\"r1\"} 7",
            "gsb_room_logic_fights_peak{room=\"r3\"} 2",
        ],
        "a MAX is a gauge, without _total"
    );
    assert_eq!(
        block("gsb_room_logic_captures_total")[..2],
        [
            "# HELP gsb_room_logic_captures_total The logic's own counter, cumulative.",
            "# TYPE gsb_room_logic_captures_total counter",
        ],
        "an empty help gets the generic one"
    );
    assert_eq!(out.matches("# TYPE gsb_room_logic_").count(), 3);
    assert!(!out.contains("room=\"r2\"} 4"), "r2 reports nothing");
}

/// The exposition with counters is the pinned text plus the logic
/// families appended, and the lines are the pinned ones plus the keys:
/// the seam adds, it never moves a core line.
#[test]
fn counters_only_append_to_the_pinned_text() {
    let mut r = golden::golden_report();
    let pinned = golden::golden_text(&r);
    r.rooms[0].logic.put(&KILLS, 3);
    let lines = r.render();
    let pinned_lines: Vec<&str> = pinned.split("\n\n").next().unwrap().lines().collect();
    assert_eq!(lines[1], format!("{} logic_kills=3", pinned_lines[1]));
    let prom = r.render_prometheus();
    let pinned_prom = pinned.split_once("\n\n").unwrap().1;
    assert_eq!(
        prom,
        format!(
            "{pinned_prom}# HELP gsb_room_logic_kills_total Players felled, cumulative.\n\
             # TYPE gsb_room_logic_kills_total counter\n\
             gsb_room_logic_kills_total{{room=\"r1\"}} 3\n"
        )
    );
}
