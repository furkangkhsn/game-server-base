//! The event families cannot drift from the counters they export: one
//! family per counter, in the counters' order, each named after its
//! field and reading that field.

use super::*;
use crate::metrics::{EventCounts, PushCounts, RouterCounts};

fn numbered<const N: usize>() -> [u64; N] {
    let mut v = [0u64; N];
    for (i, x) in v.iter_mut().enumerate() {
        *x = 100 + i as u64;
    }
    v
}

#[test]
fn every_event_counter_has_its_own_family_in_order() {
    let mut sample =
        crate::metrics::tests::room_sample(crate::id::RoomId(1), std::time::Instant::now(), 1);
    sample.events = EventCounts::from_values(numbered());
    let mut acc = crate::metrics::MetricAccumulator::default();
    acc.apply(crate::metrics::MetricsEvent::RoomFinal(sample));
    let report = acc.report(std::time::Instant::now());
    let row = report.rooms[0];
    for (f, (field, value)) in EVENTS.iter().zip(row.events.fields()) {
        assert_eq!(f.name, format!("gsb_room_{field}_total"));
        match f.value {
            super::super::RoomValue::Counter(get) => assert_eq!(get(&row), value, "{}", f.name),
            _ => panic!("{} is a counter", f.name),
        }
    }
    let mut net = report.net;
    net.pushes = PushCounts::from_values(numbered());
    for (f, (field, value)) in PUSHES.iter().zip(net.pushes.fields()) {
        assert_eq!(f.name, format!("gsb_net_{field}_total"));
        assert_eq!((f.get)(&net), value);
    }
}

#[test]
fn every_router_and_publisher_counter_has_its_own_family_in_order() {
    let mut acc = crate::metrics::MetricAccumulator::default();
    acc.apply(crate::metrics::MetricsEvent::Registry(
        crate::metrics::RegistrySample {
            events: RouterCounts::from_values(numbered()),
            ..Default::default()
        },
    ));
    let reg = acc
        .report(std::time::Instant::now())
        .registry
        .expect("a registry slice");
    for (f, (field, value)) in ROUTER.iter().zip(reg.events.fields()) {
        assert_eq!(f.name, format!("gsb_{field}_total"));
        assert_eq!((f.get)(&reg), value);
    }
    let c = PublisherCounts::from_values(numbered());
    for (f, (field, value)) in PUBLISHER.iter().zip(c.fields()) {
        assert_eq!(f.name, format!("gsb_publisher_{field}_total"));
        assert_eq!((f.get)(&c), value);
    }
}

#[test]
fn every_engine_subscriber_counter_has_its_own_family_in_order() {
    let c = crate::metrics::EngineCounts::from_values(numbered());
    for (f, (field, value)) in ENGINE.iter().zip(c.fields()) {
        assert_eq!(f.name, format!("gsb_{field}_total"));
        assert_eq!((f.get)(&c), value);
    }
}
