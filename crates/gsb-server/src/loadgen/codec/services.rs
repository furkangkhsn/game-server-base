//! The hosted services' counters (B164) on the metrics wire: the
//! report's last, variable part, its own encode/decode pair.

use gsb_core::metrics::{MethodReport, SERVICE_COUNT, ServiceCounts, ServiceReport};

use super::{R, W};

/// A name, length-prefixed (`u32`: no name is cut).
fn name(w: &mut W, s: &str) {
    w.u32(s.len() as u32);
    w.0.extend_from_slice(s.as_bytes());
}

fn read_name(r: &mut R<'_>) -> Option<String> {
    let len = usize::try_from(r.u32()?).ok()?;
    String::from_utf8(r.take(len)?.to_vec()).ok()
}

pub(super) fn encode(w: &mut W, services: &[ServiceReport]) {
    w.u32(services.len() as u32);
    for s in services {
        name(w, &s.service);
        for (_, v) in s.counts.fields() {
            w.u64(v);
        }
        w.u32(s.methods.len() as u32);
        for m in &s.methods {
            name(w, &m.method);
            w.u64(m.calls);
            w.u64(m.latency_us_total);
            w.u64(m.latency_us_max);
        }
    }
}

/// `None` on a truncated or non-UTF-8 record.
pub(super) fn decode(r: &mut R<'_>) -> Option<Vec<ServiceReport>> {
    let n = r.u32()?;
    let mut out = Vec::new();
    for _ in 0..n {
        let service = read_name(r)?;
        let mut values = [0u64; SERVICE_COUNT];
        for v in &mut values {
            *v = r.u64()?;
        }
        let m = r.u32()?;
        let mut methods = Vec::new();
        for _ in 0..m {
            methods.push(MethodReport {
                method: read_name(r)?,
                calls: r.u64()?,
                latency_us_total: r.u64()?,
                latency_us_max: r.u64()?,
            });
        }
        out.push(ServiceReport {
            service,
            counts: ServiceCounts::from_values(values),
            methods,
        });
    }
    Some(out)
}
