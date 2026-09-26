//! The team counters in the metrics sample: cumulative across the log
//! window, the logic's budget cut included.

use super::*;

/// The metrics sample carries the team counters CUMULATIVE — across the
/// ~1 s window the `team_exchange_summary` line resets (W2 promoted them
/// from that line alone): 40 ticks of one-record exports, each reporting
/// two records its logic's budget cut (A29), one import.
#[test]
fn the_sample_carries_the_team_counters_across_the_log_window() {
    let script = (0..40)
        .map(|_| {
            Some(TeamExport {
                over_budget: 2,
                ..export(&[1], &[(1, 10)])
            })
        })
        .collect();
    let mut r = rig(script, 64, true);
    let import = TeamImport {
        from: 1,
        tick: 1,
        records: vec![rec(1, 70), rec(1, 71), rec(1, 72)],
    };
    assert!(r.actor.handle_msg(ShardMsg::TeamImport(import), 1));
    for t in 1..=40 {
        assert!(r.actor.step(&tinfo(t)));
    }
    assert_eq!(exports(&mut r.registry).len(), 40);
    assert!(r.actor.border_every <= 40, "a log window passed");
    assert_eq!(r.actor.tstats_logged.exports, r.actor.border_every);
    let s = r.actor.sample();
    let got = [
        s.team_exports,
        s.team_export_records,
        s.team_export_drops,
        s.team_over_cap,
        s.team_over_budget,
        s.team_imports,
        s.team_import_records,
        s.team_expired,
    ];
    assert_eq!(got, [40, 40, 0, 0, 80, 1, 3, 0]);
    assert_eq!(r.actor.tstats_logged.over_budget, 2 * r.actor.border_every);
}

/// A budget cut counts even when nothing goes out (a logic whose budget
/// cut every record, with no viewers here).
#[test]
fn a_budget_cut_counts_without_an_export() {
    let cut = TeamExport {
        over_budget: 3,
        ..TeamExport::default()
    };
    let mut r = rig(vec![Some(cut)], 8, true);
    assert!(r.actor.step(&tinfo(1)));
    assert!(exports(&mut r.registry).is_empty(), "nothing to send");
    assert_eq!(r.actor.sample().team_over_budget, 3);
}
