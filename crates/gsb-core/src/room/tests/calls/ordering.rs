//! Ordering (SERVICES.md §5.7, spike `ordering`): `take_incoming::<R>()`
//! is per-type FIFO; `take_incoming_all()` is the tick's canonical
//! arrival order. The same five requests, two orders, both defined.

use super::*;

fn send_five(a: &Joined) {
    a.request(1, 1001, &transfer("x", 10));
    a.request(2, 1007, &Ping { n: 2 });
    a.request(3, 1001, &transfer("x", 10));
    a.request(4, 1007, &Ping { n: 4 });
    a.request(5, 1001, &transfer("x", 10));
}

fn order(seen: Vec<Seen>) -> Vec<u16> {
    seen.into_iter()
        .filter_map(|s| match s {
            Seen::Request(op, _, _) => Some(op),
            _ => None,
        })
        .collect()
}

#[test]
fn per_type_fifo_and_the_canonical_order_differ_and_are_both_defined() {
    let mut rig = Rig::new(RigCfg::default());
    let mut a = rig.join(1, "alice");
    send_five(&a);
    rig.step();
    assert_eq!(
        order(rig.seen()),
        [1001, 1001, 1001, 1007, 1007],
        "per type"
    );
    let ids: Vec<u64> = a.answers().iter().map(|x| x.id).collect();
    // Transfers 3 and 5 are BUSY (2 is in flight), the pings answered —
    // in the order the systems answered them.
    assert_eq!(ids, [3, 5, 2, 4]);

    let mut rig = Rig::new(RigCfg {
        mode: Mode {
            take_all: true,
            ..Default::default()
        },
        ..Default::default()
    });
    let mut a = rig.join(1, "alice");
    send_five(&a);
    rig.step();
    assert_eq!(
        order(rig.seen()),
        [1001, 1007, 1001, 1007, 1001],
        "canonical"
    );
    let ids: Vec<u64> = a.answers().iter().map(|x| x.id).collect();
    assert_eq!(ids, [1, 2, 3, 4, 5]);
}
