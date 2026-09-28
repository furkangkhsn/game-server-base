//! What a connection loses to a registry that has already stopped
//! (BACKLOG F54). The registry closes its mailbox in its `Shutdown` arm
//! (F53): a join queued behind that `Shutdown` is its `joins_unread`,
//! and a join sent AFTER it is refused at the connection, which answers
//! its client `ERROR` "registry gone" — counted there, once, as
//! `MetricsEvent::JoinUnsent` (the registry slice's `joins_unsent`).

use super::*;
use rig::Conn;

fn join_room_1() -> Vec<u8> {
    base::JoinRoom { room_id: 1 }.encode_to_vec()
}

/// How many `JoinUnsent` events are in `events`.
fn unsent(events: &[MetricsEvent]) -> usize {
    events
        .iter()
        .filter(|e| matches!(e, MetricsEvent::JoinUnsent))
        .count()
}

/// The registry's mailbox is closed when an authenticated connection
/// asks to join: the client is told, and the refusal is counted once.
#[tokio::test]
async fn a_join_the_stopped_registry_refused_is_counted() {
    let mut c = Conn::open(64);
    c.auth().await;
    c.registry_gone();
    c.send(op::base::JOIN_ROOM_REQ, join_room_1()).await;
    let f = c.until(op::base::ERROR).await;
    let e = base::Error::decode(&f.payload[..]).expect("ERROR decodes");
    assert!(e.message.ends_with("registry gone"), "{}", e.message);
    assert_eq!(unsent(&c.close_events().await), 1);
}

/// A join the registry takes — answered, whatever the answer — is not
/// an unsent one.
#[tokio::test]
async fn a_join_the_registry_took_is_not_counted_as_unsent() {
    let mut c = Conn::open(64);
    c.auth().await;
    c.join(4).await;
    assert_eq!(unsent(&c.close_events().await), 0);
}
