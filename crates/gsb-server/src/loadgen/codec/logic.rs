//! A room's logic counters (F9) on the metrics wire: the one variable
//! part of a room's record, so its own encode/decode pair.

use gsb_core::metrics::{LOGIC_COUNTERS_MAX, LogicCounter, LogicCounters, LogicFold};

use super::{R, W};

pub(super) fn encode(w: &mut W, logic: &LogicCounters) {
    let slots = logic.slots();
    w.u8(slots.len() as u8);
    w.u32(logic.dropped());
    for s in slots {
        let name = s.counter.name().as_bytes();
        w.u8(name.len() as u8);
        w.0.extend_from_slice(name);
        w.u8(match s.counter.fold() {
            LogicFold::Sum => 0,
            LogicFold::Max => 1,
        });
        w.u64(s.value);
    }
}

/// `None` on anything the encoder cannot have written: more counters
/// than a set holds, a name that is not a valid counter name, an
/// unknown fold rule.
pub(super) fn decode(r: &mut R<'_>) -> Option<LogicCounters> {
    let n = usize::from(r.u8()?);
    if n > LOGIC_COUNTERS_MAX {
        return None;
    }
    let mut set = LogicCounters::new();
    set.add_dropped(r.u32()?);
    for _ in 0..n {
        let len = usize::from(r.u8()?);
        let name = String::from_utf8(r.take(len)?.to_vec()).ok()?;
        let fold = match r.u8()? {
            0 => LogicFold::Sum,
            1 => LogicFold::Max,
            _ => return None,
        };
        let counter = LogicCounter::parse(&name, "", fold)?;
        set.put(&counter, r.u64()?);
    }
    Some(set)
}
