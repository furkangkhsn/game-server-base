//! The transport table cannot drift from the counters it exports: one
//! family per counter, in the counters' order, each named after its
//! field and reading that field.

use super::*;
use crate::metrics::TRANSPORT_COUNT;

#[test]
fn every_counter_has_its_own_family_in_order() {
    let mut values = [0u64; TRANSPORT_COUNT];
    for (i, v) in values.iter_mut().enumerate() {
        *v = 100 + i as u64;
    }
    let t = TransportCounters::from_values(values);
    assert_eq!(TRANSPORT.len(), TRANSPORT_COUNT);
    for (f, (field, value)) in TRANSPORT.iter().zip(t.fields()) {
        assert_eq!(f.name, format!("gsb_transport_{field}_total"));
        assert_eq!((f.get)(&t), value, "{} reads {field}", f.name);
        assert_eq!(f.kind, Kind::Counter);
    }
}
