//! The team exchange's numbers (`docs/CROSS-SHARD.md` §8b) for a game
//! whose shards export team views through the registry's hub — the war
//! (W2): the cumulative `team_*` counters every shard row carries,
//! folded (SUM) and read over the steady window as rates.

use gsb_core::metrics::RoomReport;

/// The folded report's fields the segment reads.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct TeamCounts {
    steps: u64,
    exports: u64,
    export_records: u64,
    imports: u64,
    import_records: u64,
    export_drops: u64,
    over_cap: u64,
    expired: u64,
    migrations: u64,
    effects_applied: u64,
}

impl From<&RoomReport> for TeamCounts {
    fn from(r: &RoomReport) -> Self {
        Self {
            steps: r.steps,
            exports: r.team_exports,
            export_records: r.team_export_records,
            imports: r.team_imports,
            import_records: r.team_import_records,
            export_drops: r.team_export_drops,
            over_cap: r.team_over_cap,
            expired: r.team_expired,
            migrations: r.migrations_out,
            effects_applied: r.effects_applied,
        }
    }
}

/// The RESULT segment of the team exchange: over the steady window
/// (`window`: its first and last folded reports), per SECOND at the
/// measured tick rate `hz` — exports queued on the registry's mailbox
/// (`team_exports_s`, one per shard per tick while it has team traffic),
/// their records (`team_export_records_s`) and records per export, the
/// imports that arrived (the hub's relays) and their records, and the
/// relay fan-out (imports per export); then the run's totals from
/// `total` — exports the registry mailbox refused, records the core's
/// caps cut, source slots the TTL dropped, migrations and remote effects
/// applied. Every key is always written (zeros without server reports).
pub(crate) fn team_segment(
    window: Option<(TeamCounts, TeamCounts)>,
    total: Option<TeamCounts>,
    hz: f64,
) -> String {
    let per_s = |f: fn(&TeamCounts) -> u64| -> f64 {
        match window {
            Some((a, b)) if b.steps > a.steps => {
                f(&b).saturating_sub(f(&a)) as f64 / (b.steps - a.steps) as f64 * hz
            }
            _ => 0.0,
        }
    };
    let exports = per_s(|c| c.exports);
    let records = per_s(|c| c.export_records);
    let imports = per_s(|c| c.imports);
    let ratio = |n: f64, d: f64| if d > 0.0 { n / d } else { 0.0 };
    let t = total.unwrap_or_default();
    format!(
        " team_exports_s={exports:.1} team_export_records_s={records:.0} \
         team_records_per_export={:.1} team_imports_s={imports:.1} \
         team_import_records_s={:.0} team_fanout={:.2} team_export_drops={} \
         team_over_cap={} team_expired={} migrations={} effects_applied={}",
        ratio(records, exports),
        per_s(|c| c.import_records),
        ratio(imports, exports),
        t.export_drops,
        t.over_cap,
        t.expired,
        t.migrations,
        t.effects_applied,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(steps: u64, exports: u64, export_records: u64, imports: u64) -> TeamCounts {
        TeamCounts {
            steps,
            exports,
            export_records,
            imports,
            import_records: export_records * 3,
            export_drops: 2,
            migrations: 9,
            ..TeamCounts::default()
        }
    }

    /// 30 steps at 30 Hz: 4 shards exporting every tick (120/s), 50
    /// records each, every export relayed to 3 shards.
    #[test]
    fn the_segment_reads_the_window_as_rates() {
        let (a, b) = (at(100, 400, 20_000, 1_200), at(130, 520, 26_000, 1_560));
        let s = team_segment(Some((a, b)), Some(b), 30.0);
        for kv in [
            "team_exports_s=120.0",
            "team_export_records_s=6000",
            "team_records_per_export=50.0",
            "team_imports_s=360.0",
            "team_import_records_s=18000",
            "team_fanout=3.00",
            "team_export_drops=2",
            "migrations=9",
        ] {
            assert!(
                s.contains(&format!(" {kv} ")) || s.ends_with(&format!(" {kv}")),
                "{kv}: {s}"
            );
        }
        let none = team_segment(None, None, 30.0);
        assert!(none.contains(" team_exports_s=0.0") && none.ends_with(" effects_applied=0"));
    }
}
