//! Running the real `gsb-loadgen`, and what one run can prove about the
//! room's tick rate on ANY machine (BACKLOG F25). Not a test target
//! itself — included by the loadgen suites as a plain module.
//!
//! A smoke run is a fixed wall-clock window (a few seconds), and the
//! ticker does not burst after a stall: it resyncs to the clock (see
//! `gsb_core::ticker`), so a starved process honestly steps less. The
//! suites used to assert "~30 Hz" (20–40 Hz, ≥ 60 steps, ≥ 30 snapshots
//! per client) from that window; count round 5's `otlp` run reported
//! 1.61 Hz and 0 Hz under a load average of 31–40, and freezing the
//! loadgen process 400 ms out of every 500 ms fails both demo smokes
//! every time. That lower bound measures the machine, not the engine.
//! (Nor is a per-window upper bound safe: a room thread starved while
//! the ticker is not steps the buffered ticks back to back.)
//!
//! What stays is what no stall can change:
//! - the room runs at the CONFIGURED period — its own sample echoes
//!   `budget_us` = 1/30 s (the config reached the room);
//! - the room stepped, and never faster than configured: the ticker's
//!   k-th tick is never due before k periods after it started, and a
//!   room steps at most once per tick, so a run that took `T` of wall
//!   clock holds at most `30 × T + 2` steps (one for the first tick, one
//!   for the timer's millisecond granularity) — a double-ticking room or
//!   a wrong period breaks it, a stall cannot;
//! - both measured rates reached the RESULT line as numbers;
//! - the snapshot stream reached the clients: the median client applied
//!   at least one.
//!
//! The rate itself is pinned where time is exact: a room on the live
//! ticker steps 60 times in two seconds of the paused clock
//! (`gsb-core` `room::tests::paused_clock`), and the collector turns a
//! sample pair into Δsteps/Δt (`metrics::tests`). Whether a real machine
//! achieves 30 Hz is what the manual measurement runs report.

#![allow(dead_code)]

use std::collections::HashMap;
use std::process::{Command, Output};
use std::time::{Duration, Instant};

/// The served rooms' configured rate (`RoomConfig::default().tick_hz`).
pub const CONFIGURED_HZ: f64 = 30.0;

/// One finished `gsb-loadgen` process and how long it lived.
pub struct Run {
    pub out: Output,
    pub took: Duration,
}

/// A run reads as its output (`status`, `stdout`, `stderr`).
impl std::ops::Deref for Run {
    type Target = Output;
    fn deref(&self) -> &Output {
        &self.out
    }
}

/// Run the real `gsb-loadgen` with `args` to its end.
pub fn run(args: &[&str]) -> Run {
    run_env(args, &[])
}

/// [`run`] with extra environment variables.
pub fn run_env(args: &[&str], env: &[(&str, &str)]) -> Run {
    let started = Instant::now();
    let out = Command::new(env!("CARGO_BIN_EXE_gsb-loadgen"))
        .args(args)
        .env("RUST_BACKTRACE", "1")
        .envs(env.iter().copied())
        .output()
        .expect("spawning gsb-loadgen");
    Run {
        out,
        took: started.elapsed(),
    }
}

/// Assert the stall-proof tick-rate claims of one successful run; `kv`
/// / `line` are its RESULT line.
pub fn assert_tick_rate(run: &Run, kv: &HashMap<String, String>, line: &str) {
    let num = |k: &str| -> f64 {
        kv.get(k)
            .unwrap_or_else(|| panic!("missing {k} in: {line}"))
            .parse()
            .unwrap_or_else(|_| panic!("{k} is not a number in: {line}"))
    };

    // The configured period, as the room itself reports it.
    let stdout = String::from_utf8_lossy(&run.out.stdout);
    assert_eq!(
        room_budget_us(&stdout),
        (1e6 / CONFIGURED_HZ) as u64,
        "the room's own sample must carry the configured {CONFIGURED_HZ} Hz period"
    );

    let steps = num("steps");
    let most = CONFIGURED_HZ * run.took.as_secs_f64() + 2.0;
    assert!(
        steps > 0.0 && steps <= most,
        "{steps} room steps in a {:?} run: at least one, at most {most:.0} \
         at {CONFIGURED_HZ} Hz: {line}",
        run.took
    );
    for k in ["server_hz", "tick_hz_med"] {
        let hz = num(k);
        assert!(hz.is_finite() && hz >= 0.0, "{k} = {hz}: {line}");
    }
    assert!(
        num("snap_per_client_p50") >= 1.0,
        "the median client applied no snapshot: {line}"
    );
}

/// `budget_us` of the run's `server room (final): steps=…` line.
fn room_budget_us(stdout: &str) -> u64 {
    let room = stdout
        .lines()
        .find(|l| l.starts_with("server room (final): steps="))
        .unwrap_or_else(|| panic!("no server room line in:\n{stdout}"));
    room.split_whitespace()
        .find_map(|kv| kv.strip_prefix("budget_us="))
        .and_then(|v| v.parse().ok())
        .unwrap_or_else(|| panic!("no budget_us in: {room}"))
}
