//! The publishing handles' counters (round e2, `docs/EVENTS.md` §9) on
//! the metrics wire: after the services, the report's last part, its own
//! encode/decode pair. (The room's, the router's and the connections'
//! event counters ride their own sections: fixed arrays, in the
//! counters' order.)

use gsb_core::metrics::{PUBLISHER_COUNT, PublisherCounts, PublisherReport};

use super::{R, W};

pub(super) fn encode(w: &mut W, publishers: &[PublisherReport]) {
    w.u32(publishers.len() as u32);
    for p in publishers {
        w.u32(p.publisher.len() as u32);
        w.0.extend_from_slice(p.publisher.as_bytes());
        w.u32(p.id);
        for (_, v) in p.counts.fields() {
            w.u64(v);
        }
    }
}

/// `None` on a truncated or non-UTF-8 record.
pub(super) fn decode(r: &mut R<'_>) -> Option<Vec<PublisherReport>> {
    let n = r.u32()?;
    let mut out = Vec::new();
    for _ in 0..n {
        let len = usize::try_from(r.u32()?).ok()?;
        let publisher = String::from_utf8(r.take(len)?.to_vec()).ok()?;
        let id = r.u32()?;
        let mut values = [0u64; PUBLISHER_COUNT];
        for v in &mut values {
            *v = r.u64()?;
        }
        out.push(PublisherReport {
            publisher,
            id,
            counts: PublisherCounts::from_values(values),
        });
    }
    Some(out)
}
