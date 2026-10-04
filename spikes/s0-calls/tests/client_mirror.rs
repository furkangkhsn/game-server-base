//! The client core's poll: responses and pushes are separate event kinds,
//! decoded by the same traits the server uses.

mod common;

use common::*;
use s0_calls::client::{ClientCallError, ClientCore, ClientEvent, Target};
use s0_calls::fake::FakeService;
use s0_calls::push::{Dest, Directory, Push, SessionRoute};
use s0_calls::request::ConnId;
use s0_calls::room::{Action, Inbound, Room, RoomConfig};
use s0_calls::wire;
use tokio::sync::mpsc;

#[test]
fn one_poll_yields_typed_responses_and_pushes_as_two_kinds() {
    let (econ, mut fake) = FakeService::new("economy", 8);
    let mut ledger = Ledger::with_balance(100);
    let mut room = Room::new(RoomConfig::default(), Shop::new(econ, 100), shop_table());
    let me = ConnId(1);
    let (room_in, mut private) = room.join(me, 16);
    let (client_out, mut own) = mpsc::channel(8);
    let mut dir = Directory::default();
    dir.bind(
        "me",
        SessionRoute {
            conn: me,
            push_capable: true,
            client_out,
            room_in: Some(room_in.clone()),
        },
    );

    let mut core = ClientCore::default();
    let ok = core.request(
        Target::Room,
        &BuyItem {
            item: "a".into(),
            price: 60,
        },
    );
    let no = core.request(
        Target::Room,
        &BuyItem {
            item: "b".into(),
            price: 60,
        },
    );
    for env in core.outbox.drain(..) {
        room_in
            .try_send(Inbound::Action(Action {
                conn: me,
                op: wire::op::RPC_REQ,
                payload: env,
            }))
            .unwrap();
    }
    dir.route(Push::new(
        "me",
        Dest::Client,
        &FriendOnline {
            name: "ayse".into(),
        },
    ));
    for _ in 0..3 {
        room.tick();
        fake.step(room.now(), |m, s| ledger.handle(0, m, s));
    }

    let mut events = Vec::new();
    while let Ok(f) = own.try_recv() {
        core.poll(f, &mut events);
    }
    while let Ok(f) = private.try_recv() {
        core.poll(f, &mut events);
    }
    let pushes: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            ClientEvent::Push(p) => p.decode::<FriendOnline>(),
            _ => None,
        })
        .collect();
    assert_eq!(
        pushes,
        vec![FriendOnline {
            name: "ayse".into()
        }]
    );
    let mut answered = 0;
    for e in &events {
        let ClientEvent::Response(r) = e else {
            continue;
        };
        answered += 1;
        if r.id == ok.id {
            assert_eq!(r.decode(&ok).unwrap().balance, 40);
            // The same bytes cannot be read as the other ticket's answer.
            assert_eq!(r.decode(&no), Err(ClientCallError::Mismatch));
        } else {
            assert_eq!(
                r.decode(&no),
                Err(ClientCallError::Game(EconomyError {
                    code: code::INSUFFICIENT
                }))
            );
        }
    }
    assert_eq!(answered, 2);
}
