//! A consistent cut of the EMPTY room is no population (B52): the
//! instants before the first join and after the last leave line up
//! easily, the populated ones can all be torn — and those empty cuts
//! used to become the run's population.

use super::{cut, failing_run, row};
use gsb_core::metrics::MetricReport;

use crate::report::fold::tests::report;
use crate::report::spread::{
    has_populated_cut, members_by_row, peak_population, steady_end, steady_span,
};

/// The report stream of a real failing run (`loadgen_orchestrates_the_mmo`
/// under a 128-process CPU hog, B52), row for row. The collector emits
/// once a second on the same ticker the shards sample on every 30 steps,
/// so its emit falls on (or next to) their sample tick: under load every
/// report the four bots are in is torn (1, 2) or partial (0). The only
/// consistent cuts come after the clients left — the server child runs
/// 3 s past them — and hold nobody (3, 5). They used to be the run's
/// only population: peak 0, `shard_members=0,0,0,0`, `overlap_x=0`.
fn empty_cuts_only() -> Vec<MetricReport> {
    vec![
        report(vec![row(3, 30, 0, 1)]), // partial: shard 3 alone
        cut([(30, 1), (60, 1), (30, 1), (30, 1)]),
        cut([(90, 1), (60, 1), (60, 1), (60, 1)]),
        cut([(120, 0), (120, 0), (120, 0), (120, 0)]),
        cut([(120, 0), (150, 0), (150, 0), (150, 0)]),
        cut([(150, 0), (150, 0), (150, 0), (150, 0)]),
    ]
}

/// A run whose consistent cuts are all of the empty room has no cut of
/// its population: it is read from the reports that hold players — the
/// torn fallback a run without any cut takes — not from the empty cuts.
#[test]
fn empty_cuts_do_not_stand_in_for_the_population() {
    let reports = empty_cuts_only();
    assert!(
        !has_populated_cut(&reports, 4),
        "the human block says the run is torn"
    );
    assert_eq!(
        peak_population(&reports, 4),
        4,
        "the empty cuts after the leaves must not become the population"
    );
    let (first, last) = steady_span(&reports, 4, 4).expect("a steady window");
    assert_eq!(members_by_row(first), "1,1,1,1");
    assert_eq!(members_by_row(last), "1,1,1,1");
    let end = steady_end(&reports, 4, 4).expect("a window end");
    assert_eq!(members_by_row(end), "1,1,1,1");
    assert_eq!(
        end.rooms[0].steps, 90,
        "the window ends on the last report with players"
    );
}

/// Empty cuts next to POPULATED cuts change nothing: the population is
/// still read from the consistent cuts alone, and the torn report's
/// double count (F18's 9 for 8 players) still cannot win it.
#[test]
fn empty_cuts_beside_populated_ones_keep_the_cut_rule() {
    let mut reports = vec![cut([(0, 0), (0, 0), (0, 0), (0, 0)])];
    reports.extend(failing_run());
    reports.push(cut([(150, 0), (150, 0), (150, 0), (150, 0)]));
    assert!(has_populated_cut(&reports, 4));
    assert_eq!(peak_population(&reports, 4), 8);
    let (first, last) = steady_span(&reports, 8, 4).expect("a steady window");
    assert_eq!(members_by_row(first), "2,1,4,1");
    assert_eq!(members_by_row(last), "2,1,4,1");
}
