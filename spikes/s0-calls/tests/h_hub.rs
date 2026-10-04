//! The hub: a room with no members. A request arrives with the
//! connection's own outbound path; the answer — given later, from a
//! `Done` — goes back on that path as `RPC_RESP`, never as a room
//! private frame.

mod common;

use common::*;
use s0_calls::client::{ClientCallError, ClientCore, ClientEvent, Target};
use s0_calls::fake::FakeService;
use s0_calls::request::ConnId;
use s0_calls::room::{HubRequest, Room, RoomConfig};
use s0_calls::wire::Frame;
use tokio::sync::mpsc;

#[test]
fn a_hub_request_is_answered_on_the_connections_own_path() {
    let (econ, mut fake) = FakeService::new("economy", 8);
    let mut ledger = Ledger::with_balance(300);
    let mut hub = Room::new(RoomConfig::default(), Shop::new(econ, 300), shop_table());
    let hub_in = hub.hub_inbox(64);

    // The client core (Rust SDK side) issues a typed hub request.
    let mut client = ClientCore::default();
    let ticket = client.request(
        Target::Hub,
        &BuyItem {
            item: "skin".into(),
            price: 200,
        },
    );
    let envelope = client.outbox.pop().unwrap();

    // The connection actor routes target = HUB to the hub's inbox with
    // its own outbound sender.
    let (conn_out, mut conn_rx) = mpsc::channel::<Frame>(8);
    hub_in
        .try_send(HubRequest {
            conn: ConnId(9),
            out: conn_out,
            envelope,
        })
        .unwrap();

    for _ in 0..4 {
        hub.tick();
        fake.step(hub.now(), |m, s| ledger.handle(1, m, s));
    }

    let frame = conn_rx.try_recv().expect("answered");
    assert!(matches!(frame, Frame::RpcResp(_)), "own path, not Private");
    let mut events = Vec::new();
    client.poll(frame, &mut events);
    let ClientEvent::Response(raw) = &events[0] else {
        panic!("a response kind")
    };
    assert_eq!(raw.target, Target::Hub);
    assert_eq!(raw.decode(&ticket).unwrap().balance, 100);

    // And the typed error path through the same plumbing.
    let ticket2 = client.request(
        Target::Hub,
        &BuyItem {
            item: "skin".into(),
            price: 200,
        },
    );
    let (conn_out, mut conn_rx) = mpsc::channel::<Frame>(8);
    hub_in
        .try_send(HubRequest {
            conn: ConnId(9),
            out: conn_out,
            envelope: client.outbox.pop().unwrap(),
        })
        .unwrap();
    hub.tick();
    let mut events = Vec::new();
    client.poll(conn_rx.try_recv().unwrap(), &mut events);
    let ClientEvent::Response(raw) = &events[0] else {
        panic!("a response kind")
    };
    assert_eq!(
        raw.decode(&ticket2),
        Err(ClientCallError::Game(EconomyError {
            code: code::INSUFFICIENT
        }))
    );
}
