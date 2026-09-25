//! The due schedule: the default is every step; a class's record is due
//! exactly once per period; the classes nest; and a class's records
//! are spread evenly over its period — sharded ids included.

use super::*;

const CLASSES: [SendEvery; 5] = [
    SendEvery::Tick,
    SendEvery::Ticks2,
    SendEvery::Ticks4,
    SendEvery::Ticks8,
    SendEvery::Ticks16,
];

/// The default class is every step, due on every step for every id.
#[test]
fn the_default_is_every_tick() {
    assert_eq!(SendEvery::default(), SendEvery::Tick);
    let periods: Vec<u64> = CLASSES.iter().map(|c| c.ticks()).collect();
    assert_eq!(periods, [1, 2, 4, 8, 16]);
    for wire in [0, 1, 2, 3, 1 << 20, u64::MAX] {
        assert!((0..100).all(|s| SendEvery::Tick.due(s, wire)));
    }
}

/// Exactly one due step in every window of a period (so a change waits
/// at most `ticks() − 1` steps), and a step due for a class is due for
/// every smaller class (the nesting that keeps the bound across class
/// changes).
#[test]
fn a_record_is_due_once_per_period_and_the_classes_nest() {
    for wire in (1..2_000).chain([u64::MAX - 7, u64::MAX]) {
        for c in CLASSES {
            for start in 0..40 {
                let due = (start..start + c.ticks())
                    .filter(|&s| c.due(s, wire))
                    .count();
                assert_eq!(due, 1, "{c:?} wire {wire} from step {start}");
            }
        }
        for pair in CLASSES.windows(2) {
            for s in 0..64 {
                if pair[1].due(s, wire) {
                    assert!(pair[0].due(s, wire), "{pair:?} wire {wire} step {s}");
                }
            }
        }
    }
    // Across the wrap of the step counter too.
    assert_eq!(
        (u64::MAX - 7..=u64::MAX)
            .filter(|&s| SendEvery::Ticks8.due(s, 42))
            .count(),
        1
    );
}

/// A class's records are due evenly across its period: consecutive ids
/// (a single room's minter) and the arithmetic progressions a sharded
/// room's shards mint (A30's interleaved ids — congruent mod the shard
/// count) alike. Per due step, within a quarter of the even share.
#[test]
fn a_class_is_spread_over_its_period() {
    let mut streams: Vec<(String, Vec<u64>)> = vec![("1..=800".into(), (1..=800).collect())];
    for shards in [2u64, 3, 4, 8] {
        for i in 0..shards {
            let ids = (1..=800).map(|n| (n - 1) * shards + i + 1).collect();
            streams.push((format!("shard {i} of {shards}"), ids));
        }
    }
    for (name, ids) in &streams {
        for c in &CLASSES[1..] {
            let share = ids.len() as f64 / c.ticks() as f64;
            for step in 0..c.ticks() {
                let due = ids.iter().filter(|&&w| c.due(step, w)).count() as f64;
                assert!(
                    (due - share).abs() <= share / 4.0,
                    "{name}, {c:?}, step {step}: {due} due of an even {share}"
                );
            }
        }
    }
}
