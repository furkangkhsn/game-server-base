//! What a connection loses to a registry that has already stopped
//! (BACKLOG F54). The registry closes its mailbox in its `Shutdown` arm
//! (F53): a join queued behind that `Shutdown` is its `joins_unread`,
//! and a join sent AFTER it is refused at the connection, which answers
//! its client `ERROR` "registry gone" — counted there, once, as
//! `MetricsEvent::JoinUnsent` (the registry slice's `joins_unsent`).
//! And a verdict the stop's notice overtook in the connection's inbox is
//! a lost verdict (F56, `MetricsEvent::VerdictsLost`).

use super::*;
use gsb_core::conn::ServerClose;
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

/// The lost verdicts among `events` (F56), merged.
fn lost(events: &[MetricsEvent]) -> gsb_core::metrics::VerdictsLost {
    let mut all = gsb_core::metrics::VerdictsLost::default();
    for e in events {
        if let MetricsEvent::VerdictsLost(v) = e {
            all.add(v);
        }
    }
    all
}

fn kick() -> ConnIn {
    ConnIn::ServerClosed {
        cause: ServerClose::Kicked,
        reason: "kicked: afk".into(),
    }
}

/// A room's kick relayed to the connection BEHIND the stop's notice (the
/// test queues both before the actor reads): the stop ends the session,
/// the client gets ERROR 14 instead of the kick's ERROR 9, and the kick
/// is a lost verdict — once, however many verdicts wait behind (a
/// session books one reason). Before F56 it was counted nowhere.
#[tokio::test]
async fn a_verdict_behind_the_stop_is_a_lost_verdict() {
    let mut c = Conn::open(64);
    c.auth().await;
    c.tell(ConnIn::Shutdown).await;
    c.tell(kick()).await;
    c.tell(ConnIn::RoomGone(gsb_core::id::RoomId(1))).await;
    let v = lost(&c.close_events().await);
    assert_eq!(v.closes.get(ServerClose::Kicked), 1);
    assert_eq!(v.closes.total(), 1, "one per session");
    assert_eq!((v.leaves, v.detach_despawns), (0, 0));
}

/// The same kick AHEAD of the stop is booked, not lost; and behind a
/// client's own end it loses nothing (the session had already ended).
#[tokio::test]
async fn a_verdict_ahead_of_the_stop_or_behind_a_client_end_is_not_lost() {
    let mut c = Conn::open(64);
    c.auth().await;
    c.tell(kick()).await;
    c.tell(ConnIn::Shutdown).await;
    c.actor_done().await;
    let mut events = Vec::new();
    while let Some(ev) = c.take_metric() {
        events.push(ev);
    }
    let booked = events
        .iter()
        .any(|e| matches!(e, MetricsEvent::Conn(s) if s.server_close == Some(ServerClose::Kicked)));
    assert!(booked, "the kick is booked");
    assert!(lost(&events).is_empty());

    let c = Conn::open(64);
    c.tell(ConnIn::Closed {
        reason: "peer left".into(),
    })
    .await;
    c.tell(kick()).await;
    assert!(lost(&c.close_events().await).is_empty());
}
