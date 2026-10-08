//! Engine-originated presence → a friends service → FriendOnline /
//! FriendOffline (CLIENT) to the friends (docs/EVENTS.md §5.4, §8.3),
//! live: the router and the service are tokio tasks, each one loop with
//! one await.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use e0_events::client::EventsClient;
use e0_events::conn::ConnActor;
use e0_events::contract::{ConnId, Identity};
use e0_events::engine::{EngineEvent, Interest};
use e0_events::game::{FriendOffline, FriendOnline};
use e0_events::publisher::Publisher;
use e0_events::router::{ConnLink, Router, RouterMsg};
use e0_events::wire::{FEATURE_PUSH, Frame};
use tokio::sync::mpsc;

/// The game's friends service. In the engine it is a
/// `#[gsb_core::service]` whose `#[gsb(engine_events)]` method receives
/// these; here it is the same loop written out.
struct Friends {
    graph: BTreeMap<Identity, Vec<Identity>>,
    online: BTreeSet<Identity>,
    events: Publisher,
}

impl Friends {
    fn online_friends(&self, of: &Identity) -> Vec<Identity> {
        self.graph.get(of).into_iter().flatten().filter(|f| self.online.contains(*f)).cloned().collect()
    }

    fn on_engine(&mut self, e: EngineEvent) {
        match e {
            EngineEvent::Online { identity } | EngineEvent::Resumed { identity, .. } => {
                if !self.online.insert(identity.clone()) {
                    return;
                }
                let to = self.online_friends(&identity);
                let _ = self.events.push_many(&to, &FriendOnline { who: identity.as_str().into() });
                for f in &to {
                    let _ = self.events.push(&identity, &FriendOnline { who: f.as_str().into() });
                }
            }
            EngineEvent::Offline { identity } | EngineEvent::Parked { identity, .. } if self.online.remove(&identity) => {
                let to = self.online_friends(&identity);
                let _ = self.events.push_many(&to, &FriendOffline { who: identity.as_str().into() });
            }
            _ => {}
        }
    }

    async fn run(mut self, mut rx: mpsc::Receiver<EngineEvent>) {
        while let Some(e) = rx.recv().await {
            self.on_engine(e);
        }
    }
}

struct Session {
    actor: ConnActor,
    frames: mpsc::Receiver<Frame>,
    client: EventsClient,
    link: ConnLink,
}

fn session(n: u64) -> Session {
    let (out, frames) = mpsc::channel(64);
    let (actor, inbox) = ConnActor::new(ConnId(n), 64, out);
    Session {
        actor,
        frames,
        client: EventsClient::default(),
        link: ConnLink {
            id: ConnId(n),
            inbox,
            features: FEATURE_PUSH,
        },
    }
}

fn id(s: &str) -> Identity {
    Identity::new(s)
}

async fn settle(sessions: &mut BTreeMap<&'static str, Session>, mut done: impl FnMut(&mut BTreeMap<&'static str, Session>) -> bool) {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            for s in sessions.values_mut() {
                s.actor.pump();
                while let Ok(f) = s.frames.try_recv() {
                    s.client.ingest(&f);
                }
            }
            if done(sessions) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .expect("settled");
}

#[tokio::test(flavor = "current_thread")]
async fn presence_flows_from_the_engine_through_a_service_to_friends() {
    let mut router = Router::new(256);
    let presence = router.subscribe("friends", Interest::PRESENCE, 64);
    let mut graph = BTreeMap::new();
    graph.insert(id("alice"), vec![id("bob"), id("carol")]);
    graph.insert(id("bob"), vec![id("alice")]);
    graph.insert(id("carol"), vec![id("alice")]);
    let friends = Friends {
        graph,
        online: BTreeSet::new(),
        events: router.publisher(),
    };
    let registry = router.sender(); // the registry's notices
    let mut sessions: BTreeMap<&'static str, Session> = BTreeMap::new();
    for (i, n) in ["alice", "bob", "carol"].into_iter().enumerate() {
        sessions.insert(n, session(i as u64 + 1));
    }
    let router_task = tokio::spawn(router.run());
    let friends_task = tokio::spawn(friends.run(presence));

    for n in ["bob", "carol", "alice"] {
        let conn = sessions[n].link.clone();
        registry.send(RouterMsg::Online { identity: id(n), conn }).await.unwrap();
    }
    settle(&mut sessions, |s| {
        s.get_mut("alice").unwrap().client.counts().received >= 2 && s.get_mut("carol").unwrap().client.counts().received >= 1
    })
    .await;
    let mut to_alice: Vec<String> = sessions.get_mut("alice").unwrap().client.take::<FriendOnline>().into_iter().map(|e| e.body.who).collect();
    to_alice.sort();
    assert_eq!(to_alice, ["bob", "carol"], "alice learns who is on");
    for n in ["bob", "carol"] {
        let got = sessions.get_mut(n).unwrap().client.take::<FriendOnline>();
        assert_eq!(got.iter().map(|e| e.body.who.as_str()).collect::<Vec<_>>(), ["alice"], "{n}");
    }

    let conn = sessions["alice"].link.id;
    registry.send(RouterMsg::Offline { identity: id("alice"), conn }).await.unwrap();
    settle(&mut sessions, |s| s.get_mut("bob").unwrap().client.counts().received >= 2).await;
    let off = sessions.get_mut("bob").unwrap().client.take::<FriendOffline>();
    assert_eq!(off[0].body.who, "alice");
    assert!(sessions.get_mut("bob").unwrap().client.take_gaps().is_empty());

    router_task.abort();
    friends_task.abort();
}

#[test]
fn a_full_subscriber_is_counted_per_name() {
    let mut r = Router::new(16);
    let _rx = r.subscribe("slow", Interest::ONLINE, 1);
    let _all = r.subscribe("joins", Interest::JOINED, 8);
    for (i, n) in ["a", "b", "c"].into_iter().enumerate() {
        let s = session(i as u64);
        r.handle(RouterMsg::Online { identity: id(n), conn: s.link });
    }
    let c = r.subscribers().counts("slow").unwrap();
    assert_eq!((c.sent, c.refused_full), (1, 2));
    assert_eq!(r.subscribers().counts("joins").unwrap().sent, 0, "interest filters");
}
