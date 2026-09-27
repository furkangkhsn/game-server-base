//! READ's binding translation drops what a connection with no binding
//! row sent (a stale session — structurally rare: the old channel dies
//! with the rebind), and counts it by kind (BACKLOG B54): an RPC request
//! as `requests_dropped_unbound` (a term of the RPC ledger), a plain
//! game action as `actions_dropped_unbound`.

use super::*;

fn put(tx: &Mailbox<Action>, conn: u64, op: u16) {
    tx.try_send(Action {
        conn: ConnectionId(conn),
        player: PlayerId(0),
        op,
        payload: bytes::Bytes::new(),
    })
    .expect("room in the channel");
}

/// Three frames under an unbound connection (one request, two actions)
/// and one under the bound one ride the same channel: the three are
/// dropped at the translation and counted by kind, the fourth is not.
#[test]
fn what_an_unbound_connection_sent_is_counted_by_kind() {
    let mut r = room(Detach::Despawn);
    let tx = join(&mut r, 1, "");
    put(&tx, 99, crate::rpc::RPC_REQ_OP);
    put(&tx, 99, 0x2001);
    put(&tx, 99, 0x2001);
    put(&tx, 1, 0x2001);
    assert!(r.step(&TickInfo {
        tick: 1,
        at: Instant::now(),
    }));
    assert_eq!(r.m.requests_dropped_unbound, 1);
    assert_eq!(r.m.actions_dropped_unbound, 2);
    assert_eq!(r.m.actions_dropped_unread, 0, "pulled, not left unread");
    let s = r.sample();
    assert_eq!(
        (s.requests_dropped_unbound, s.actions_dropped_unbound),
        (1, 2)
    );
}
