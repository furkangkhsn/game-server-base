//! The input-idle ceiling's action (BACKLOG E6) against the real server,
//! over TCP: a client that keeps its transport alive with heartbeats but
//! sends no game input is, under `afk_action = "disconnect"`, told
//! ERROR 9 (`input idle: …`) and closed — booked as `idle_input` — while
//! the default leaves it connected (it only loses its room membership).
//! A game's default (`GameModule::afk_action`) reaches every room under
//! the operator's key.

use std::sync::Arc;
use std::time::{Duration, Instant};

use bevy_ecs::world::World;
use gsb_client::conn::{Conn, Recv};
use gsb_client::session::{self, Credentials};
use gsb_core::conn::ServerClose;
use gsb_core::metrics::MetricReport;
use gsb_core::registry::{BuiltRoom, RoomFactory};
use gsb_core::room::{AfkAction, RoomLogic};
use gsb_demo::prelude::*;
use gsb_protocol::{MessageTable, base, op};
use gsb_server::{Config, GameModule, RegistryParts, RegistryTask, ServerError};
use prost::Message;
use tokio::sync::mpsc;

const W: Duration = Duration::from_secs(10);

fn local(afk_action: Option<AfkAction>) -> Config {
    Config {
        bind: "127.0.0.1:0".into(),
        room_count: 1,
        max_idle_input_secs: Some(1),
        afk_action,
        ..Config::default()
    }
}

async fn player(addr: std::net::SocketAddr, name: &str) -> Conn {
    let mut c = gsb_client::connect::tcp(addr).await.expect("connect");
    session::auth_and_join(&mut c, &Credentials::named(name), 1, W, |_| {})
        .await
        .unwrap_or_else(|e| panic!("{name}: join failed: {e}"));
    c
}

/// Heartbeat every 200 ms for `span`, collecting every ERROR frame; stop
/// early at the close. Returns the ERRORs and whether the server closed.
async fn heartbeat_for(c: &mut Conn, span: Duration) -> (Vec<base::Error>, bool) {
    let mut errors = Vec::new();
    let end = Instant::now() + span;
    let mut tick = 0;
    while Instant::now() < end {
        tick += 1;
        let hb = session::heartbeat(tick);
        if c.send(hb.op, &hb.payload).await.is_err() {
            return (errors, true);
        }
        let until = Instant::now() + Duration::from_millis(200);
        while let Some(left) = until.checked_duration_since(Instant::now()) {
            match c.recv(left).await {
                Ok(Recv::Frame(f)) if f.op == op::base::ERROR => {
                    errors.push(base::Error::decode(&f.payload[..]).expect("ERROR decodes"));
                }
                Ok(Recv::Frame(_)) => {}
                Ok(Recv::Quiet) => break,
                Ok(Recv::Closed) | Err(_) => return (errors, true),
            }
        }
    }
    (errors, false)
}

/// The server-close vector once it counts `want` closes.
async fn closes(rx: &mut mpsc::UnboundedReceiver<MetricReport>, want: u64) -> MetricReport {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let left = deadline
            .checked_duration_since(Instant::now())
            .expect("the close was never counted");
        let r = tokio::time::timeout(left, rx.recv())
            .await
            .expect("a report in time")
            .expect("reports open");
        if r.net.server_closes.total() >= want {
            return r;
        }
    }
}

/// `disconnect`: a heartbeating client that sends no game input gets
/// ERROR 9 naming the ceiling, then the close; the close is counted as
/// `idle_input` and as nothing else.
#[tokio::test]
async fn disconnect_closes_a_heartbeating_idle_client() {
    let (tx, mut reports) = mpsc::unbounded_channel();
    let handle = gsb_server::start_server_metrics(local(Some(AfkAction::Disconnect)), tx)
        .await
        .expect("server starts");
    assert_eq!(handle.room_config(1).afk_action, AfkAction::Disconnect);
    let mut c = player(handle.addr, "idle").await;
    let (errors, closed) = heartbeat_for(&mut c, Duration::from_secs(6)).await;
    assert!(closed, "the server closed the connection");
    assert_eq!(errors.len(), 1, "{errors:?}");
    assert_eq!(errors[0].code(), base::ErrorCode::ServerClosed);
    assert!(errors[0].message.starts_with("input idle"), "{errors:?}");
    let r = closes(&mut reports, 1).await;
    for (reason, n) in r.net.server_closes.iter() {
        assert_eq!(n, u64::from(reason == ServerClose::IdleInput), "{reason:?}");
    }
    handle.stop().await;
}

/// The default: the same idle client stays connected, and nothing is
/// sent to it — its heartbeats keep being answered.
#[tokio::test]
async fn the_default_keeps_a_heartbeating_idle_client_connected() {
    let (tx, _reports) = mpsc::unbounded_channel();
    let handle = gsb_server::start_server_metrics(local(None), tx)
        .await
        .expect("server starts");
    assert_eq!(handle.room_config(1).afk_action, AfkAction::LeaveRoom);
    let mut c = player(handle.addr, "idle").await;
    let (errors, closed) = heartbeat_for(&mut c, Duration::from_secs(3)).await;
    assert!(!closed && errors.is_empty(), "{errors:?}");
    session::heartbeat_round(&mut c, 99, W, |_| {})
        .await
        .expect("still connected");
    handle.stop().await;
}

/// A game module that kicks idle players from the server by default.
struct Kicking;

impl GameModule for Kicking {
    fn name(&self) -> &'static str {
        "kicking"
    }
    fn configure(&mut self, _raw: &toml::Table, _engine: &Config) -> Result<(), ServerError> {
        Ok(())
    }
    fn register(&self, table: &mut MessageTable) {
        gsb_demo::register(table);
    }
    fn spawn_registry(&self, parts: RegistryParts) -> RegistryTask {
        let factory: RoomFactory<World, (), (), ()> = Arc::new(|_id, _config| BuiltRoom::Single {
            world: World::new(),
            logic: Box::new(gsb_demo::room::OpenRoom::new())
                as Box<dyn RoomLogic<World, GroupKey = (), Strip = ()>>,
        });
        parts.spawn(factory)
    }
    fn describe(&self) -> String {
        "kicking".into()
    }
    fn afk_action(&self) -> AfkAction {
        AfkAction::Disconnect
    }
}

/// The game's default is every room's action, under the operator's key;
/// `Config::room_config` stays the file's view.
#[tokio::test]
async fn a_game_default_reaches_the_rooms_under_the_operator_key() {
    for (key, want) in [
        (None, AfkAction::Disconnect),
        (Some(AfkAction::LeaveRoom), AfkAction::LeaveRoom),
    ] {
        let cfg = Config {
            game: "kicking".into(),
            ..local(key)
        };
        let handle = gsb_server::start_game_server(Box::new(Kicking), cfg.clone())
            .await
            .expect("the module starts");
        assert_eq!(handle.room_config(1).afk_action, want, "{key:?}");
        assert_eq!(handle.room_config(7).afk_action, want, "{key:?}: any id");
        assert_eq!(
            cfg.room_config(1).afk_action,
            key.unwrap_or_default(),
            "the file's view carries no game default"
        );
        handle.stop().await;
    }
}
