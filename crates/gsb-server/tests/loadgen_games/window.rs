//! The claims the per-game smokes read from a WINDOW of the run, not
//! from its totals (BACKLOG F30): a sharded room's population
//! (`shard_members=`, the last consistent cut at the run's peak —
//! `loadgen/report/spread.rs`) and the war's team exchange (`team_*_s`,
//! over the steady window that opens at the first report past 100
//! steps — `loadgen/report/team.rs`).
//!
//! Both need the server to have sampled while the players were in: the
//! rooms sample every 30 steps and the collector reports once a second.
//! A starved run can end without such a sample — frozen 400 ms out of
//! every 500 ms (the F25 method) the room steps at about 8 Hz, a 3 s run
//! holds no sample with the players in (`shard_members=0,0,0,0`) and an
//! 8 s one never passes 100 steps (no window, every rate 0). The claim is
//! then neither true nor false there: the run holds no evidence for it.
//!
//! So a window claim is read from a CONCLUSIVE run. A run without the
//! evidence is repeated with twice the duration (a longer run steps
//! more, stalled or not); the claim is asserted, exactly as before, on
//! the first run that has it, and a run that has it and contradicts the
//! claim fails at once. The doubling stops at [`MOST_SECS`]: a hang
//! guard, not part of the claim — a claim whose evidence never comes
//! fails there. Every run, conclusive or not, passes the line's own
//! checks (`result`, the stall-proof tick-rate claims).

use std::collections::HashMap;

use super::loadgen_rate::{self, Run};

/// The longest run the doubling reaches.
pub const MOST_SECS: u64 = 64;

/// Run the real `gsb-loadgen` with `args` and `--duration secs`, then
/// twice as long, and so on, until `read` finds the evidence in a run
/// (`Ok`); `Err` names what a run lacked.
pub fn conclusive<T>(
    args: &[&str],
    secs: u64,
    mut read: impl FnMut(&Run) -> Result<T, String>,
) -> T {
    let mut secs = secs;
    let mut lacked = Vec::new();
    loop {
        let duration = secs.to_string();
        let mut line = args.to_vec();
        line.extend(["--duration", duration.as_str()]);
        match read(&loadgen_rate::run(&line)) {
            Ok(found) => return found,
            Err(why) => lacked.push(format!("{secs} s: {why}")),
        }
        secs *= 2;
        assert!(
            secs <= MOST_SECS,
            "no run up to {} s held the evidence: {lacked:#?}",
            secs / 2
        );
    }
}

/// The per-shard population of a run whose `n` players were all caught
/// by one consistent cut — the room at one instant (`spread.rs`). `Err`
/// for a run without such a cut: every report with players in it torn
/// (the human block says so), or every cut at the peak short of `n` (a
/// player in flight between two shards, or not yet in). A cut that
/// counts MORE than `n` is a player counted twice at one instant: no
/// stall can do that, it fails at once.
pub fn population(stdout: &str, kv: &HashMap<String, String>, n: u32) -> Result<Vec<u32>, String> {
    let line = &kv["shard_members"];
    let shards = stdout
        .lines()
        .find(|l| l.starts_with("server shards (members per shard"))
        .ok_or_else(|| format!("no per-shard population (shard_members={line})"))?;
    if shards.contains("(torn") {
        return Err(format!("no consistent cut with players in it: {shards}"));
    }
    let members: Vec<u32> = line
        .split(',')
        .map(|m| m.parse().expect("a member count"))
        .collect();
    let sum: u32 = members.iter().sum();
    assert!(
        sum <= n,
        "a consistent cut counts {sum} of {n} players: {shards}"
    );
    if sum < n {
        return Err(format!("no cut caught all {n} players: {shards}"));
    }
    Ok(members)
}

/// The team exchange over the steady window, per STEP (scale-free: a
/// starved room steps less, not differently — the per-second rates are
/// these times the measured tick rate).
#[derive(Debug, Clone, Copy)]
pub struct TeamWindow {
    /// Exports queued per step, over all the shards.
    pub exports: f64,
    /// Imports (the hub's relays) arrived per step.
    pub imports: f64,
}

/// [`TeamWindow`] of a run that has a steady window; `Err` for a run
/// without one. The window is the overlap measurement's too, so
/// `records_per_tick > 0` says it exists (the war always ships records).
pub fn team_window(kv: &HashMap<String, String>) -> Result<TeamWindow, String> {
    let num = |k: &str| -> f64 { kv[k].parse().unwrap_or_else(|_| panic!("{k}: a number")) };
    if num("records_per_tick") <= 0.0 {
        return Err("no steady window (no report past 100 steps before the last full one)".into());
    }
    let hz = num("server_hz");
    assert!(hz > 0.0, "a steady window at a server rate of {hz}");
    Ok(TeamWindow {
        exports: num("team_exports_s") / hz,
        imports: num("team_imports_s") / hz,
    })
}
