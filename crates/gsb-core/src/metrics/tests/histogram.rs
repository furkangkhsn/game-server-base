//! The two step-duration histograms: budget-ratio binning and the
//! fine sub-budget resolution with its percentile reader.

use super::*;

#[test]
fn hist_index_binning() {
    // Budget-relative bins (budget = 1024 µs ⇒ clean integer edges
    // 8,16,32,64,128,256,512,1024,2048,4096,8192,16384,32768):
    // [0,8) [8,16) [16,32) [32,64) [64,128) [128,256) [256,512)
    // [512,1024) [1024,2048) [2048,4096) [4096,8192) [8192,16384)
    // [16384,32768) [32768,∞).
    const B: u64 = 1024;
    assert_eq!(hist_index(B, 0), 0);
    assert_eq!(hist_index(B, 7), 0);
    assert_eq!(hist_index(B, 8), 1); // 1/128× budget
    assert_eq!(hist_index(B, 15), 1);
    assert_eq!(hist_index(B, 16), 2); // 1/64× budget
    assert_eq!(hist_index(B, 511), 6); // 1/2×, under budget
    assert_eq!(hist_index(B, 1023), 7); // just under budget
    // The tick budget itself is the overflow boundary.
    assert_eq!(hist_index(B, 1024), HIST_OVERFLOW_BIN);
    assert_eq!(hist_index(B, 2047), HIST_OVERFLOW_BIN); // 1-2× budget
    assert_eq!(hist_index(B, 2048), 9); // 2× budget
    assert_eq!(hist_index(B, 4096), 10); // 4× budget
    assert_eq!(hist_index(B, 32768), 13); // 32× budget → top bin
    assert_eq!(hist_index(B, u64::MAX), HIST_BINS - 1);

    // The spec's own example: at a 30 Hz budget (33 333 µs) a 5.1 ms step
    // (inside the budget) and a 40 ms step (over it) must land in
    // *different* bins, and the 40 ms one must be readable as overflow.
    assert_ne!(hist_index(33_333, 5_100), hist_index(33_333, 40_000));
    assert_eq!(hist_index(33_333, 40_000), HIST_OVERFLOW_BIN);
    assert!(hist_index(33_333, 5_100) < HIST_OVERFLOW_BIN);
}

/// The fine histogram resolves sub-budget changes the log2 one cannot:
/// a 10% step-time difference at ~390 µs is TWO different fine p50
/// values, while both are the SAME log2 bin (both report the same
/// coarse "~391" midpoint).
#[test]
fn fine_hist_resolves_ten_percent_difference() {
    let mut a = [0u64; FINE_HIST_BINS];
    let mut b = [0u64; FINE_HIST_BINS];
    a[fine_hist_index(390).unwrap()] = 1000;
    b[fine_hist_index(430).unwrap()] = 1000; // +10.3% step time
    let pa = fine_hist_percentile_us(&a, 1000, 50).unwrap();
    let pb = fine_hist_percentile_us(&b, 1000, 50).unwrap();
    assert_eq!(pa, 390 / 8 * 8); // bin lower edge
    assert_eq!(pb, 430 / 8 * 8);
    assert_ne!(pa, pb, "the fine histogram must separate a 10% change");
    // ...and the log2 histogram does NOT (same bin at a 30 Hz budget).
    let lo_a = hist_index(33_333, 390);
    let lo_b = hist_index(33_333, 430);
    assert_eq!(lo_a, lo_b, "sanity: the log2 bins are 2× apart here");
}

/// Known distributions → exact percentiles (integer arithmetic; the
/// answer is the bin lower edge, so the error is < FINE_HIST_US_PER_BIN).
#[test]
fn fine_hist_percentiles_known_distributions() {
    // Uniform over [0, 4096): 2 steps per bin, 1000 total.
    let mut u = [0u64; FINE_HIST_BINS];
    for bin in u.iter_mut() {
        *bin = 2;
    }
    // The 500th of 1000 steps: bin 249 (500 steps in bins 0..=249).
    assert_eq!(fine_hist_percentile_us(&u, 1000, 50), Some(249 * 8));
    assert_eq!(fine_hist_percentile_us(&u, 1000, 99), Some(989 / 2 * 8));
    // Bimodal: 50% at 390 µs, 50% at 780 µs (the measured clusters).
    let mut m = [0u64; FINE_HIST_BINS];
    m[fine_hist_index(390).unwrap()] = 500;
    m[fine_hist_index(780).unwrap()] = 500;
    assert_eq!(
        fine_hist_percentile_us(&m, 1000, 50),
        Some(390 / 8 * 8),
        "p50: the 500th step is the last one in the 390 cluster"
    );
    assert_eq!(
        fine_hist_percentile_us(&m, 1000, 99),
        Some(780 / 8 * 8),
        "p99: inside the 780 cluster"
    );
    // Degenerate: all one value.
    let mut d = [0u64; FINE_HIST_BINS];
    d[fine_hist_index(1234).unwrap()] = 77;
    assert_eq!(fine_hist_percentile_us(&d, 77, 1), Some(1234 / 8 * 8));
    assert_eq!(fine_hist_percentile_us(&d, 77, 100), Some(1234 / 8 * 8));
}

/// Cap semantics: steps at/above FINE_HIST_CAP_US are absent from the
/// fine histogram (the log2 histogram keeps the overflow signal), and a
/// percentile whose rank falls beyond the cap reports `None`.
#[test]
fn fine_hist_cap_and_overflow() {
    assert_eq!(fine_hist_index(0), Some(0));
    assert_eq!(fine_hist_index(FINE_HIST_CAP_US - 1), Some(FINE_HIST_BINS - 1));
    assert_eq!(fine_hist_index(FINE_HIST_CAP_US), None);
    assert_eq!(fine_hist_index(u64::MAX), None);

    // 100 steps below the cap (last fine bin), 50 at/above it: the p50
    // is in the fine range, the p99 is not.
    let mut h = [0u64; FINE_HIST_BINS];
    h[FINE_HIST_BINS - 1] = 100;
    assert_eq!(
        fine_hist_percentile_us(&h, 150, 50),
        Some((FINE_HIST_BINS - 1) as u64 * FINE_HIST_US_PER_BIN)
    );
    assert_eq!(fine_hist_percentile_us(&h, 150, 99), None);

    // Empty / out-of-range p.
    let z = [0u64; FINE_HIST_BINS];
    assert_eq!(fine_hist_percentile_us(&z, 0, 50), None);
    assert_eq!(fine_hist_percentile_us(&h, 150, 0), None);
    assert_eq!(fine_hist_percentile_us(&h, 150, 101), None);
}
