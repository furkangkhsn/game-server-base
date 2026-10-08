//! The demo's event in a real flow (round e5, `docs/EVENTS.md` §8): a
//! purchase the economy confirms is granted — ONE `ItemGranted` pushed
//! to the buyer (`to: BOTH`), the buyer's identity being the session's
//! (the room buys on its behalf). Its WORLD leg reaches the buyer's room
//! at a later step's EVENTS and the demo counts the item on the buyer's
//! entity (`Granted`); a second purchase is a second grant, a newer
//! version.

use std::time::Duration;

use gsb_core::calls::{ServiceClient, WireMessage};
use gsb_core::determinism::{Sim, SimRoom};
use gsb_core::events::{Dest, Target};
use gsb_core::id::RoomId;
use gsb_core::room::RoomConfig;

use super::*;
use crate::demo::components::{Account, Granted};
use crate::demo::economy::{Economy, EconomyHandle, EconomyService, PriceList};
use crate::demo::game::{BuyItem, ItemGranted};
use crate::demo::op;
use crate::room::OpenRoom;

fn granted(room: &SimRoom<World>) -> Option<(String, Granted)> {
    room.world().iter_entities().find_map(|e| {
        let account = e.get::<Account>()?;
        Some((
            account.0.as_str().to_owned(),
            e.get::<Granted>().copied().unwrap_or_default(),
        ))
    })
}

#[test]
fn a_purchase_grants_the_item_to_the_buyer() {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_time()
        .build()
        .expect("runtime");
    let _in = rt.enter();
    let mut sim = Sim::new();
    let grants = sim.publisher("economy");
    let (route, _svc) = gsb_core::calls::spawn(
        Economy::serve(PriceList::new(Duration::ZERO).with_publisher(grants.clone())),
        |s| s.mailbox(8),
    );
    let economy = EconomyService::from_handle(EconomyHandle::from_route(route));
    let config = RoomConfig {
        id: RoomId(1),
        ..Default::default()
    };
    let mut room = SimRoom::new(
        config,
        World::new(),
        Box::new(OpenRoom::new().with_economy(economy)),
    );
    let alice = room.join(1, "alice");
    assert_eq!(
        granted(&room),
        Some(("alice".into(), Granted::default())),
        "the identity is stamped at spawn"
    );
    for round in 1..=2u64 {
        buy_potion(&alice, round);
        let mut steps = 0;
        while granted(&room).map(|(_, g)| g.items) != Some(round as u32) {
            steps += 1;
            assert!(steps < 500, "the grant arrives");
            room.tick(&mut sim);
            std::thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(
            granted(&room),
            Some((
                "alice".into(),
                Granted {
                    items: round as u32,
                    version: round
                }
            ))
        );
    }
    // A grant no newer than the last (a duplicate) counts nothing.
    let alice_id = gsb_core::calls::Identity::new("alice");
    let again = ItemGranted {
        kind: "potion".into(),
        version: 2,
    };
    grants.push(&alice_id, &again).expect("pushed");
    room.tick(&mut sim);
    room.tick(&mut sim);
    assert_eq!(granted(&room).map(|(_, g)| g.items), Some(2), "a duplicate");
    let c = room.event_counts();
    assert_eq!((c.events_admitted, c.events_taken), (3, 3), "{c:?}");
    assert_eq!(grants.stats().counts().events_pushed, 3);
    let pushes = &sim.pushes()[..2];
    for (i, p) in pushes.iter().enumerate() {
        assert_eq!((p.op(), p.dest()), (op::ITEM_GRANTED, Dest::Both));
        assert!(matches!(p.target(), Target::One(id) if id.as_str() == "alice"));
        let body = ItemGranted::from_wire(p.body()).expect("ItemGranted");
        assert_eq!(body.kind, "potion");
        assert_eq!(body.version, i as u64 + 1);
    }
}

/// Alice buys a potion (request `id`).
fn buy_potion(alice: &gsb_core::determinism::SimClient, id: u64) {
    alice.request(
        id,
        &BuyItem {
            kind: "potion".into(),
        },
    );
}
