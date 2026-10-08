//! Engine events through a real server (round e4, `docs/EVENTS.md`
//! §5.4): a game module subscribes a hosted `#[gsb_core::service]`
//! (`RegistryParts::service` with `Setup::engine_events`) and a plain
//! receiver (`RegistryParts::engine_events`), and takes the presence
//! query handle (`RegistryParts::presence`); a client over TCP comes
//! online and goes away, and each subscriber hears it.

use std::sync::Arc;
use std::time::Duration;

use gsb_client::session::{self, Credentials};
use gsb_core::calls::Identity;
use gsb_core::events::{EngineEvent, EngineNotice, Interest, Presence, PresenceState};
use gsb_core::registry::RoomFactory;
use gsb_protocol::MessageTable;
use gsb_server::{Config, GameModule, RegistryParts, RegistryTask, ServerError};
use tokio::sync::mpsc;

const WAIT: Duration = Duration::from_secs(10);

/// What the test hears: which subscriber, what.
type Heard = (&'static str, EngineEvent);

/// The hosted subscriber.
#[gsb_core::service]
pub trait Watch {
    /// A call beside the engine method (a service is never only that).
    async fn ping(&self, cx: &gsb_core::calls::CallCx) -> Result<(), ()>;
    #[gsb(engine_events)]
    async fn on_engine(&self, n: EngineNotice);
}

struct Watcher {
    heard: mpsc::UnboundedSender<Heard>,
}

impl Watch for Watcher {
    async fn ping(&self, _cx: &gsb_core::calls::CallCx) -> Result<(), ()> {
        Ok(())
    }
    async fn on_engine(&self, n: EngineNotice) {
        let _ = self.heard.send(("Watch", n.event));
    }
}

struct Module {
    heard: mpsc::UnboundedSender<Heard>,
    presence: mpsc::UnboundedSender<Presence>,
}

impl GameModule for Module {
    fn name(&self) -> &'static str {
        "engine-events"
    }
    fn configure(&mut self, _raw: &toml::Table, _engine: &Config) -> Result<(), ServerError> {
        Ok(())
    }
    fn register(&self, _table: &mut MessageTable) {}
    fn spawn_registry(&self, mut parts: RegistryParts) -> RegistryTask {
        parts
            .service(
                Watch::serve(Watcher {
                    heard: self.heard.clone(),
                }),
                |s| s.engine_events(Interest::PRESENCE),
            )
            .expect("hosted once");
        let mut audit = parts.engine_events("audit", Interest::ONLINE, 16);
        let heard = self.heard.clone();
        tokio::spawn(async move {
            while let Some(n) = audit.recv().await {
                let _ = heard.send(("audit", n.event));
            }
        });
        let _ = self.presence.send(parts.presence());
        // `room_count = 0`: no room is ever built.
        let factory: RoomFactory<(), (), (), ()> =
            Arc::new(|_id, _cfg| unreachable!("no room in this test"));
        parts.spawn(factory)
    }
    fn describe(&self) -> String {
        "engine-events: a hosted and a plain subscriber".into()
    }
}

async fn next(rx: &mut mpsc::UnboundedReceiver<Heard>) -> Heard {
    tokio::time::timeout(WAIT, rx.recv())
        .await
        .expect("heard in time")
        .expect("open")
}

#[tokio::test]
async fn a_hosted_service_and_a_plain_receiver_hear_a_client_come_and_go() {
    let (heard_tx, mut heard) = mpsc::unbounded_channel();
    let (presence_tx, mut presence) = mpsc::unbounded_channel();
    let cfg = Config {
        bind: "127.0.0.1:0".into(),
        room_count: 0,
        ..Default::default()
    };
    let module = Module {
        heard: heard_tx,
        presence: presence_tx,
    };
    let handle = gsb_server::start_game_server(Box::new(module), cfg)
        .await
        .expect("the module starts");
    let presence = presence.recv().await.expect("the module's handle");
    let mut conn = gsb_client::connect::tcp(handle.addr)
        .await
        .expect("connect");
    session::auth(&mut conn, &Credentials::named("e4-1"), WAIT, |_| {})
        .await
        .unwrap_or_else(|e| panic!("auth failed: {e}"));
    let me = Identity::new("e4-1");
    let mut got = vec![next(&mut heard).await, next(&mut heard).await];
    got.sort_by_key(|h| h.0);
    let online = EngineEvent::Online {
        identity: me.clone(),
    };
    assert_eq!(got, [("Watch", online.clone()), ("audit", online)]);
    let now = presence.of(&[me.clone()]).await.expect("answered");
    assert_eq!(now[0].state, PresenceState::Live);

    drop(conn);
    assert_eq!(
        next(&mut heard).await,
        ("Watch", EngineEvent::Offline { identity: me })
    );
    let _ = handle.stop().await;
}
