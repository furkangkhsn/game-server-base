//! The shard's other session ends that take an action channel along
//! (BACKLOG B36; the leave and the superseding rejoin are locked end to
//! end in `tests/rpc_shard/unread.rs`): a resume swapping in a fresh
//! channel, and a migration of a session that already left. Each counts
//! the RPC requests still unread in the channel it drops — and only the
//! requests.

use super::*;

// READ's binding translation drops, counted by kind (B54).
mod unbound;
// What the shard still holds when it stops, counted (B62).
mod stop;

/// Two requests and one plain action into a session's channel.
fn two_requests_and_an_action(tx: &Mailbox<Action>, conn: ConnectionId) {
    for op in [crate::rpc::RPC_REQ_OP, 0x2001, crate::rpc::RPC_REQ_OP] {
        tx.try_send(Action {
            conn,
            player: PlayerId(0),
            op,
            payload: bytes::Bytes::new(),
        })
        .expect("room in the channel");
    }
}

/// A resume rebinds the row onto a fresh channel: the old session's
/// unread requests are counted as the old channel goes.
#[tokio::test]
async fn a_resume_counts_the_old_channels_unread_requests() {
    let mut a = bare_shard(0);
    let (reply_tx, reply_rx) = oneshot::channel();
    let (out_tx, _out_rx) = mpsc::channel::<FrameBatch>(8);
    assert!(a.handle_msg(
        ShardMsg::Join {
            conn: ConnectionId(1),
            epoch: 1,
            identity: String::new(),
            out: out_tx.clone(),
            reply: reply_tx
        },
        1
    ));
    let (_entity, old) = reply_rx.await.expect("delivered").expect("admitted");
    two_requests_and_an_action(&old, ConnectionId(1));
    let player = a.binding[&ConnectionId(1)];
    let _fresh = a.rebind_session(player, ConnectionId(2), 2, "", out_tx);
    assert_eq!(
        a.m.requests_dropped_unread, 2,
        "the requests, not the action"
    );
    assert_eq!(a.m.actions_dropped_unread, 1, "the action, apart (B54)");
    assert!(old.is_closed(), "the old channel is gone");
}

/// A migration that arrives after its session's leave is dropped at the
/// epoch gate, and the channel it carried with it: its unread requests
/// are counted there.
#[tokio::test]
async fn a_dead_migration_counts_the_channels_unread_requests() {
    let mut a = bare_shard(1);
    let conn = ConnectionId(6);
    let wire = join_direct(&mut a, conn, 2, 100).await;
    assert!(a.handle_msg(
        ShardMsg::Leave {
            conn,
            entity: wire,
            epoch: 2
        },
        101
    ));
    let before = a.m.requests_dropped_unread;

    let (tx, rx) = mpsc::channel::<Action>(8);
    two_requests_and_an_action(&tx, conn);
    let mut ghost = ghost_migrate(conn, 2, wire, 99);
    if let ShardMsg::Migrate {
        player: Some(p), ..
    } = &mut ghost
    {
        p.actions = rx;
    }
    assert!(a.handle_msg(ghost, 102));
    assert!(
        !a.conns.contains_key(&PlayerId(conn.0)),
        "the dead join stays out"
    );
    assert_eq!(a.m.requests_dropped_unread - before, 2);
    assert_eq!(a.m.actions_dropped_unread, 1, "the action, apart (B54)");
}
