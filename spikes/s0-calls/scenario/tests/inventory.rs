//! The four inventory patterns, on bevy function systems (the scenario
//! in `common::bevy_game`). Timeline (sim: inventory 2 ticks, bank 3,
//! friends 1):
//!
//! - t0 alice and bob join → `load` per player (pattern 1), answered t2;
//! - t4 both pick up loot 1 → alice claims it (pending state ON the loot
//!   entity, the call's owner), bob is refused at once; granted t6
//!   (pattern 2);
//! - t5 alice crafts from two 1s → both locked, answered t7 through
//!   `.answers` (pattern 3); bob equips → answered at once, marked dirty,
//!   autosaved at t9 (pattern 4); bob picks up loot 2, which expires at
//!   t6 → the grant comes back to a gone owner at t7 (counted; bob is
//!   still answered).

mod common;

use bevy_ecs::prelude::*;
use common::*;
use s0_calls_bevy_ecs::{BevyRoom, EntityExt, GsbCalls};

fn bag(room: &mut Room<BevyRoom>, conn: ConnId) -> (Entity, Bag) {
    let w = &mut room.logic_mut().world;
    w.query::<(Entity, &Player, &Bag)>()
        .iter(w)
        .find(|(_, p, _)| p.conn == conn)
        .map(|(e, _, b)| (e, b.clone()))
        .expect("player")
}

fn loot_pile(room: &mut Room<BevyRoom>, id: u64) -> Option<Loot> {
    let w = &mut room.logic_mut().world;
    w.query::<&Loot>().iter(w).find(|l| l.id == id).cloned()
}

fn in_flight<C: Call>(room: &mut Room<BevyRoom>, owner: Owner) -> bool {
    room.logic_mut()
        .world
        .resource::<GsbCalls>()
        .in_flight::<C>(owner)
}

#[test]
fn the_four_inventory_patterns() {
    let be = FakeBackend::new();
    let mut sim = sim(&be);
    let mut room = bevy_game::room(RoomConfig::default(), Clients::from(&sim));
    let mut all = Answers::new();
    let tick = |room: &mut Room<BevyRoom>, sim: &mut Sim, t: u64, all: &mut Answers| {
        script(t, &room.inputs());
        for (c, r) in room.tick_sim(sim) {
            all.push((t, c, r));
        }
    };

    // Pattern 1: load on join.
    tick(&mut room, &mut sim, 0, &mut all);
    let (a, b0) = bag(&mut room, ALICE);
    assert!(!b0.loaded);
    assert!(in_flight::<inventory::Load>(&mut room, a.to_owner()));
    tick(&mut room, &mut sim, 1, &mut all);
    assert!(!bag(&mut room, ALICE).1.loaded);
    tick(&mut room, &mut sim, 2, &mut all);
    let (_, b2) = bag(&mut room, ALICE);
    assert!(b2.loaded);
    assert_eq!(b2.kinds(), vec![1, 1, 2]);
    assert!(!in_flight::<inventory::Load>(&mut room, a.to_owner()));

    // Pattern 2: loot pickup, pending state on the loot entity.
    tick(&mut room, &mut sim, 3, &mut all);
    tick(&mut room, &mut sim, 4, &mut all);
    let pile = loot_pile(&mut room, 1).expect("still there while pending");
    assert!(pile.claimed, "claimed by alice, pending");
    let bob_says = answer(&all, BOB, 21);
    assert_eq!(bob_says.len(), 1);
    assert_eq!(bob_says[0].0, 4, "the loser hears at once");
    assert_eq!(decode::<GameError>(&bob_says[0].2).code, CLAIMED);

    // Pattern 3 + 4 issued at t5.
    tick(&mut room, &mut sim, 5, &mut all);
    let (_, b5) = bag(&mut room, ALICE);
    let locked: Vec<_> = b5
        .items
        .iter()
        .filter(|i| i.locked)
        .map(|i| i.kind)
        .collect();
    assert_eq!(locked, vec![1, 1], "ingredients locked while crafting");
    let eq = answer(&all, BOB, 40);
    assert_eq!(eq[0].0, 5, "equip needs no service: answered the same tick");
    assert!(bag(&mut room, BOB).1.dirty);

    tick(&mut room, &mut sim, 6, &mut all);
    assert!(
        loot_pile(&mut room, 1).is_none(),
        "granted: the loot is gone"
    );
    let (_, b6) = bag(&mut room, ALICE);
    let held = |b: &Bag| b.items.iter().filter(|i| i.kind == pile.item).count();
    assert_eq!(held(&b6), held(&b5) + 1, "alice holds the loot");
    let got = answer(&all, ALICE, 20);
    assert_eq!(got[0].0, 6);
    assert_eq!(decode::<PickedUp>(&got[0].2).item, pile.item);

    tick(&mut room, &mut sim, 7, &mut all);
    let (_, b7) = bag(&mut room, ALICE);
    assert!(b7.items.iter().all(|i| !i.locked));
    assert!(b7.has_unlocked(3), "crafted item arrived");
    let crafted = answer(&all, ALICE, 30);
    assert_eq!(crafted[0].0, 7, "answered by .answers");
    assert_eq!(decode::<Crafted>(&crafted[0].2).item, 3);
    assert_eq!(be.count("alice", 3), 1);

    // Owner gone: loot 2 expired at t6, its grant came back at t7.
    let bob_loot2 = answer(&all, BOB, 22);
    assert_eq!(bob_loot2[0].0, 7);
    assert!(bob_loot2[0].2.ok, "the client is still answered");
    assert_eq!(room.counters().responses_owner_gone, 1);

    // Pattern 4: autosave at t9 for both dirty players, one in flight each.
    for t in 8..SCENARIO_TICKS {
        tick(&mut room, &mut sim, t, &mut all);
        if t == 9 {
            let (b, _) = bag(&mut room, BOB);
            assert!(in_flight::<inventory::Save>(&mut room, b.to_owner()));
            assert!(!bag(&mut room, BOB).1.dirty);
        }
    }
    assert_eq!(be.0.saves.load(std::sync::atomic::Ordering::Relaxed), 2);
    assert_eq!(
        be.0.equipped[1].load(std::sync::atomic::Ordering::Relaxed),
        2
    );

    let c = room.counters();
    assert_eq!(c.responses_unhandled, 0, "every result was taken");
    assert_eq!(c.requests_unanswered + c.requests_untaken, 0);
    assert_eq!(c.calls_issued, c.results_ok);
}
