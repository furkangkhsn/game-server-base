//! The call table cannot drift from the counters it exports: one family
//! per counter, in the counters' order, each named after its field and
//! reading that field.

use super::*;
use crate::metrics::{CALL_COUNT, CallCounts};

#[test]
fn every_call_counter_has_its_own_family_in_order() {
    let mut values = [0u64; CALL_COUNT];
    for (i, v) in values.iter_mut().enumerate() {
        *v = 100 + i as u64;
    }
    let mut sample =
        crate::metrics::tests::room_sample(crate::id::RoomId(1), std::time::Instant::now(), 1);
    sample.calls = CallCounts::from_values(values);
    let mut acc = crate::metrics::MetricAccumulator::default();
    acc.apply(crate::metrics::MetricsEvent::RoomFinal(sample));
    let row = acc.report(std::time::Instant::now()).rooms[0];
    assert_eq!(CALLS.len(), CALL_COUNT);
    for (f, (field, value)) in CALLS.iter().zip(row.calls.fields()) {
        assert_eq!(f.name, format!("gsb_room_{field}_total"));
        match f.value {
            super::super::RoomValue::Counter(get) => assert_eq!(get(&row), value, "{}", f.name),
            _ => panic!("{} is a counter", f.name),
        }
    }
}
