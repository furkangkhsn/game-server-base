//! Ordering: `take_incoming::<R>()` is per-type FIFO; `take_incoming_all`
//! is the tick's canonical arrival order (READ's order, the order the
//! input log records). Across types, separate `take_incoming` calls see
//! requests in SYSTEM order — that is the documented trade.

mod common;

use common::*;

#[derive(Default)]
struct S {
    all: bool,
    log: Vec<(u16, u64)>,
}

fn sys(s: &mut S, calls: &mut Calls, _ctx: &mut TickCtx) {
    if s.all {
        for r in calls.take_incoming_all() {
            let op = r.op();
            match r.downcast::<Equip>() {
                Ok(e) => {
                    s.log.push((op, u64::from(e.body.item)));
                    calls.reply(e.reply, Ok(Equipped { item: e.body.item }));
                }
                Err(r) => match r.downcast::<PickUp>() {
                    Ok(p) => {
                        s.log.push((op, p.body.loot));
                        calls.reply(p.reply, Err(err(NO_SUCH_LOOT)));
                    }
                    Err(_) => unreachable!(),
                },
            }
        }
    } else {
        // Two "systems": pickups first, then equips.
        for p in calls.take_incoming::<PickUp>() {
            s.log.push((PickUp::OP, p.body.loot));
            calls.reply(p.reply, Err(err(NO_SUCH_LOOT)));
        }
        for e in calls.take_incoming::<Equip>() {
            s.log.push((Equip::OP, u64::from(e.body.item)));
            calls.reply(e.reply, Ok(Equipped { item: e.body.item }));
        }
    }
}

fn run(all: bool) -> Vec<(u16, u64)> {
    let mut room = scripted(
        RoomConfig::default(),
        S {
            all,
            log: Vec::new(),
        },
        |c| {
            c.accept::<Equip>().unwrap();
            c.accept::<PickUp>().unwrap();
        },
        sys,
    );
    let i = room.inputs();
    i.join(ALICE, "alice");
    i.join(BOB, "bob");
    i.request(ALICE, rpc(1, &Equip { item: 1 }));
    i.request(BOB, rpc(2, &PickUp { loot: 1 }));
    i.request(ALICE, rpc(3, &Equip { item: 2 }));
    i.request(BOB, rpc(4, &PickUp { loot: 2 }));
    i.request(ALICE, rpc(5, &Equip { item: 3 }));
    let out = room.tick(0.0);
    assert_eq!(out.len(), 5, "all answered");
    room.logic().game.s.log.clone()
}

#[test]
fn per_type_fifo_versus_canonical_order() {
    let (e, p) = (Equip::OP, PickUp::OP);
    assert_eq!(
        run(false),
        vec![(p, 1), (p, 2), (e, 1), (e, 2), (e, 3)],
        "each type in arrival order; types in system order"
    );
    assert_eq!(
        run(true),
        vec![(e, 1), (p, 1), (e, 2), (p, 2), (e, 3)],
        "take_incoming_all: the tick's arrival order"
    );
}
