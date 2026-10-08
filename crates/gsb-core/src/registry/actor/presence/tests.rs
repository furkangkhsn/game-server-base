//! The presence derivation (`changes`) case by case, and the registry's
//! own loop answering presence questions and emitting what a session's
//! AUTH and close change. The flows through rooms (join, leave, park,
//! resume) run live in `crates/gsb-core/tests/engine_events.rs`.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc;

use super::{View, changes};
use crate::calls::Identity;
use crate::channel::{Inbox, Mailbox, channel};
use crate::conn::ConnIn;
use crate::events::{
    EngineEvent, EngineNotice, EngineReceiver, EngineSubscribers, Interest, Presence, PresencePage,
    PresenceRefused, PresenceState,
};
use crate::id::{ConnectionId, RoomId};
use crate::registry::actor::Registry;
use crate::registry::{RegistryMsg, RoomFactory};
use crate::ticker::Ticker;

const WAIT: Duration = Duration::from_secs(5);

fn id(s: &str) -> Identity {
    Identity::new(s)
}

fn v(live: bool, room: Option<u64>) -> Option<View> {
    Some(View {
        live,
        room: room.map(RoomId),
    })
}

#[test]
fn the_changes_between_two_views_are_named_in_order() {
    let a = id("a");
    let online = EngineEvent::Online {
        identity: a.clone(),
    };
    let offline = EngineEvent::Offline {
        identity: a.clone(),
    };
    let joined = |r| EngineEvent::Joined {
        identity: a.clone(),
        room: RoomId(r),
    };
    let left = |r| EngineEvent::Left {
        identity: a.clone(),
        room: RoomId(r),
    };
    let parked = |r: Option<u64>| EngineEvent::Parked {
        identity: a.clone(),
        room: r.map(RoomId),
    };
    let resumed = |r: Option<u64>| EngineEvent::Resumed {
        identity: a.clone(),
        room: r.map(RoomId),
    };
    let cases: Vec<(Option<View>, Option<View>, Vec<EngineEvent>)> = vec![
        (None, None, vec![]),
        (None, v(true, None), vec![online.clone()]),
        (None, v(true, Some(1)), vec![online.clone(), joined(1)]),
        // A first row that is not live (never produced today) still
        // reports where it waits.
        (
            None,
            v(false, Some(1)),
            vec![online.clone(), joined(1), parked(Some(1))],
        ),
        (v(true, None), None, vec![offline.clone()]),
        (v(false, Some(2)), None, vec![left(2), offline.clone()]),
        (v(true, None), v(true, Some(1)), vec![joined(1)]),
        (v(true, Some(1)), v(true, None), vec![left(1)]),
        (v(true, Some(1)), v(true, Some(2)), vec![left(1), joined(2)]),
        (v(true, Some(1)), v(false, Some(1)), vec![parked(Some(1))]),
        (v(true, None), v(false, None), vec![parked(None)]),
        (v(false, Some(1)), v(true, Some(1)), vec![resumed(Some(1))]),
        // A resume that lands in another room in the same message (not
        // one today): the room change first.
        (
            v(false, Some(1)),
            v(true, Some(2)),
            vec![left(1), joined(2), resumed(Some(2))],
        ),
        (v(true, Some(3)), v(true, Some(3)), vec![]),
    ];
    for (before, after, want) in cases {
        assert_eq!(changes(&a, before, after), want, "{before:?} -> {after:?}");
    }
}

/// A running registry (no room: the factory is never called) offering
/// engine events to `subs`; its mailbox.
fn registry(subs: EngineSubscribers) -> Mailbox<RegistryMsg> {
    let (ticker, _task) = Ticker::spawn(60.0, 64).expect("valid tick rate");
    let factory: RoomFactory<(), (), (), ()> =
        Arc::new(|_id, _cfg| unreachable!("no room is created here"));
    let (tx, rx) = channel::<RegistryMsg>(64);
    let (metrics, mut sink) = mpsc::channel(1024);
    tokio::spawn(async move { while sink.recv().await.is_some() {} });
    let reg: Registry<(), (), (), ()> =
        Registry::new(rx, tx.clone(), factory, ticker, metrics, None, None, None)
            .with_engine_events(subs);
    tokio::spawn(reg.run());
    tx
}

/// Open connection `c` and authenticate it as `name`; the returned inbox
/// keeps its row's transport up.
async fn login(reg: &Mailbox<RegistryMsg>, c: u64, name: &str) -> Inbox<ConnIn> {
    let (inbox, rx) = channel::<ConnIn>(8);
    let conn = ConnectionId(c);
    reg.send(RegistryMsg::ConnOpened {
        conn,
        inbox,
        source: None,
    })
    .await
    .expect("registry alive");
    reg.send(RegistryMsg::Authed {
        conn,
        identity: name.into(),
        features: 0,
    })
    .await
    .expect("registry alive");
    rx
}

async fn close(reg: &Mailbox<RegistryMsg>, c: u64) {
    reg.send(RegistryMsg::ConnClosed {
        conn: ConnectionId(c),
        verdict: None,
    })
    .await
    .expect("registry alive");
}

async fn next(rx: &mut EngineReceiver) -> EngineNotice {
    tokio::time::timeout(WAIT, rx.recv())
        .await
        .expect("a notice in time")
        .expect("subscribed")
}

/// Online at the first session only, offline at the last one's close;
/// anonymous sessions produce nothing; a subscriber hears only its
/// interest, numbered from 1 on its own.
#[tokio::test]
async fn online_and_offline_follow_an_identitys_rows() {
    let mut subs = EngineSubscribers::new();
    let mut all = subs.subscribe("all", Interest::ALL, 64);
    let mut offline = subs.subscribe("off", Interest::OFFLINE, 64);
    let reg = registry(subs);
    let _a1 = login(&reg, 1, "alice").await;
    let _a2 = login(&reg, 2, "alice").await; // a second session: no news
    let _anon = login(&reg, 3, "").await;
    close(&reg, 1).await;
    close(&reg, 3).await;
    close(&reg, 2).await;
    let n = next(&mut all).await;
    let online = EngineEvent::Online {
        identity: id("alice"),
    };
    assert_eq!((n.seq, n.event), (1, online));
    let n = next(&mut all).await;
    let gone = EngineEvent::Offline {
        identity: id("alice"),
    };
    assert_eq!((n.seq, n.event), (2, gone));
    let n = next(&mut offline).await;
    assert_eq!(n.seq, 1, "a subscriber's own numbers");
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert!(all.try_recv().is_err() && offline.try_recv().is_err());
}

fn names(p: &PresencePage) -> Vec<String> {
    p.entries
        .iter()
        .map(|e| e.identity.as_str().to_owned())
        .collect()
}

/// The registry answers presence questions from its table: pages in
/// identity order with a cursor, and each asked identity.
#[tokio::test]
async fn presence_pages_and_lookups_come_from_the_table() {
    let reg = registry(EngineSubscribers::new());
    let mut keep = Vec::new();
    for (c, n) in ["dave", "bob", "erin", "alice", "carol"].iter().enumerate() {
        keep.push(login(&reg, c as u64 + 1, n).await);
    }
    let presence = Presence::new(reg.clone());
    let p = presence.page(None, 2).await.expect("answered");
    assert_eq!(names(&p), ["alice", "bob"]);
    assert_eq!(p.next, Some(id("bob")));
    let p = presence.page(p.next.as_ref(), 2).await.expect("answered");
    assert_eq!(names(&p), ["carol", "dave"]);
    let p = presence.page(p.next.as_ref(), 2).await.expect("answered");
    assert_eq!(names(&p), ["erin"]);
    assert_eq!(p.next, None, "the last page");
    assert!(p.entries.iter().all(|e| e.state == PresenceState::Live));
    let p = presence.page(None, 5).await.expect("answered");
    assert_eq!(p.entries.len(), 5);
    assert_eq!(p.next, None, "exactly a page: no cursor");
    let got = presence
        .of(&[id("carol"), id("zed")])
        .await
        .expect("answered");
    assert_eq!(
        got.iter().map(|e| e.state).collect::<Vec<_>>(),
        [PresenceState::Live, PresenceState::Offline]
    );
    assert_eq!(got[1].identity, id("zed"));
    // A gone registry refuses.
    reg.send(RegistryMsg::Shutdown)
        .await
        .expect("registry alive");
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(presence.of(&[id("bob")]).await, Err(PresenceRefused::Gone));
}
