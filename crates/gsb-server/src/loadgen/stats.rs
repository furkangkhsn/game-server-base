//! Small statistics over the collected reports: medians,
//! percentiles, and the budget-overshoot fraction.

use gsb_core::metrics::{HIST_OVERFLOW_BIN, hist_edge_us};

use crate::client::*;

/// The client's measured server tick rate (the snapshot sequence is the
/// global tick index).
pub(crate) fn measured_hz(r: &ClientReport) -> Option<f64> {
    let ((f, t1), (l, t2)) = (r.seq_first?, r.seq_last?);
    if l <= f {
        return None;
    }
    let dt = t2.duration_since(t1).as_secs_f64();
    if dt < 0.5 {
        return None;
    }
    Some((l - f) as f64 / dt)
}

pub(crate) fn median(v: &[f64]) -> f64 {
    if v.is_empty() {
        return 0.0;
    }
    let mut s = v.to_vec();
    s.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    s[s.len() / 2]
}

pub(crate) fn pctl(v: &mut [u128], p: f64) -> u128 {
    if v.is_empty() {
        return 0;
    }
    v.sort();
    v[(v.len() as f64 * p).min(v.len() as f64 - 1.0) as usize]
}

/// Approximate percentile of a step-duration histogram (bin midpoints;
/// the top bin uses the observed max). The bins are fractions of the room's
/// tick budget (`budget_us`), so the result is in µs and the overflow
/// boundary (the budget) is meaningful.
pub(crate) fn hist_percentile(hist: &[u64], budget_us: u64, max_us: u64, p: f64) -> f64 {
    let total: u64 = hist.iter().sum();
    if total == 0 {
        return 0.0;
    }
    let target = total as f64 * p;
    let mut acc = 0u64;
    for (i, &n) in hist.iter().enumerate() {
        acc += n;
        if acc as f64 >= target {
            let lo = if i == 0 {
                0
            } else {
                hist_edge_us(budget_us, i - 1)
            };
            let hi = if i < hist.len() - 1 {
                hist_edge_us(budget_us, i)
            } else {
                max_us.max(lo + 1)
            };
            return (lo as f64 + hi as f64) / 2.0;
        }
    }
    max_us as f64
}

/// Fraction of steps that exceed the tick budget (bins >= HIST_OVERFLOW_BIN),
/// i.e. the "room cannot keep its rate" mass — now readable from the
/// budget-relative histogram (A1).
pub(crate) fn over_budget_frac(hist: &[u64]) -> f64 {
    let total: u64 = hist.iter().sum();
    if total == 0 {
        return 0.0;
    }
    hist.iter().skip(HIST_OVERFLOW_BIN).sum::<u64>() as f64 / total as f64
}
