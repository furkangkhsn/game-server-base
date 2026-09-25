//! The receiver's slots (`TeamImports`): wholesale replacement per
//! source, the per-team merge and its dedup rule, the TTL, the caps.

use super::*;

fn rec(team: u64, wire: u64, body: &'static [u8]) -> TeamRecord {
    TeamRecord {
        team,
        wire,
        bytes: Bytes::from_static(body),
    }
}

fn import(from: usize, tick: u64, records: Vec<TeamRecord>) -> TeamImport {
    TeamImport {
        from,
        tick,
        records,
    }
}

/// `(wire, body, from, tick)` of `team`'s merged list.
fn view(imports: &TeamImports, team: u64) -> Vec<(u64, Vec<u8>, usize, u64)> {
    imports
        .team(team)
        .iter()
        .map(|r| (r.wire, r.bytes.to_vec(), r.from, r.tick))
        .collect()
}

/// An import replaces its source's slot WHOLESALE: a record the new
/// import no longer names is gone at once (no TTL wait), another
/// source's slot is untouched, and an empty import clears the slot.
#[test]
fn an_import_replaces_its_sources_slot_wholesale() {
    let mut im = TeamImports::default();
    im.insert(import(1, 10, vec![rec(0, 5, b"a"), rec(0, 6, b"b")]));
    im.insert(import(2, 10, vec![rec(0, 7, b"c")]));
    im.settle();
    assert_eq!(
        im.team(0).iter().map(|r| r.wire).collect::<Vec<_>>(),
        [5, 6, 7]
    );

    im.insert(import(1, 11, vec![rec(0, 6, b"b2")]));
    im.settle();
    assert_eq!(
        view(&im, 0),
        [(6, b"b2".to_vec(), 1, 11), (7, b"c".to_vec(), 2, 10)],
        "wire 5 left with source 1's new set; source 2 kept its own"
    );

    im.insert(import(1, 12, Vec::new()));
    im.settle();
    assert_eq!(view(&im, 0), [(7, b"c".to_vec(), 2, 10)]);
    assert_eq!(im.sources(), 1);
}

/// Isolation at the receiver: a team's list holds that team's records
/// only — the same wire may be visible to two teams (an enemy both see)
/// and is then listed under each.
#[test]
fn each_team_reads_only_its_own_records() {
    let mut im = TeamImports::default();
    im.insert(import(
        1,
        4,
        vec![rec(0, 5, b"a"), rec(1, 6, b"b"), rec(1, 5, b"a")],
    ));
    im.settle();
    assert_eq!(im.team(0).iter().map(|r| r.wire).collect::<Vec<_>>(), [5]);
    assert_eq!(
        im.team(1).iter().map(|r| r.wire).collect::<Vec<_>>(),
        [5, 6]
    );
    assert!(im.team(2).is_empty(), "a team nobody exported sees nothing");
    assert_eq!(im.teams().collect::<Vec<_>>(), [0, 1]);
    assert_eq!(im.len(), 3);
}

/// Two sources name one wire (a member crossing a seam is in both
/// exports for a tick): ONE record per team, the newest tick's; a tie
/// goes to the lower source index.
#[test]
fn a_wire_two_sources_name_is_listed_once_newest_first() {
    let mut im = TeamImports::default();
    im.insert(import(3, 20, vec![rec(0, 9, b"old")]));
    im.insert(import(1, 21, vec![rec(0, 9, b"new")]));
    im.settle();
    assert_eq!(view(&im, 0), [(9, b"new".to_vec(), 1, 21)]);

    let mut tie = TeamImports::default();
    tie.insert(import(3, 20, vec![rec(0, 9, b"three")]));
    tie.insert(import(1, 20, vec![rec(0, 9, b"one")]));
    tie.settle();
    assert_eq!(view(&tie, 0), [(9, b"one".to_vec(), 1, 20)]);
}

/// A source silent for the TTL is dropped (no ghost outlives it); one
/// tick short of it the slot stands. `expire` counts only slots that
/// still held records.
#[test]
fn a_silent_source_expires_after_the_ttl() {
    let mut im = TeamImports::default();
    im.insert(import(1, 100, vec![rec(0, 5, b"a")]));
    im.insert(import(2, 100, Vec::new()));
    assert_eq!(im.expire(100 + TEAM_EXPORT_TTL_TICKS - 1), 0);
    im.settle();
    assert_eq!(im.len(), 1, "one tick short of the TTL: still there");
    assert_eq!(
        im.expire(100 + TEAM_EXPORT_TTL_TICKS),
        1,
        "the emptied slot is not a ghost"
    );
    im.settle();
    assert!(im.is_empty(), "the TTL removed the ghost");
    assert_eq!(im.sources(), 0);
}

/// An import older than the slot's current one never overwrites it.
#[test]
fn an_older_import_never_overwrites_a_newer_slot() {
    let mut im = TeamImports::default();
    im.insert(import(1, 30, vec![rec(0, 5, b"new")]));
    im.insert(import(1, 29, vec![rec(0, 6, b"old")]));
    im.settle();
    assert_eq!(view(&im, 0), [(5, b"new".to_vec(), 1, 30)]);
}

/// A slot never holds more than the hard cap; the cut is reported.
#[test]
fn a_slot_holds_at_most_the_cap() {
    let mut im = TeamImports::default();
    let many: Vec<TeamRecord> = (0..TEAM_EXPORT_MAX_RECORDS as u64 + 7)
        .map(|w| rec(0, w, b"x"))
        .collect();
    assert_eq!(im.insert(import(1, 1, many)), 7);
    im.settle();
    assert_eq!(im.len(), TEAM_EXPORT_MAX_RECORDS);
}
