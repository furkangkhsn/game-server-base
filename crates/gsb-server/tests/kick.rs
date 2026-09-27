//! The game's kick verb (BACKLOG E8) against the real server, over TCP:
//! a room logic that kicks a player through its tick context
//! (`TickCtx::kick`) closes that player's connection — the client reads
//! ERROR 9 carrying `kicked: <the game's reason>`, then the close — and
//! the server books it as `kicked` and as nothing else. A bystander in
//! the same room plays on.

use std::sync::Arc;
use std::time::{Duration, Instant};

use bevy_ecs::world::World;
use gsb_client::ClientError;
use gsb_client::conn::{Conn, Recv};
use gsb_client::session::{self, Credentials};
use gsb_core::conn::ServerClose;
use gsb_core::id::{ConnectionId, PlayerId};
use gsb_core::metrics::MetricReport;
use gsb_core::registry::{BuiltRoom, RoomFactory};
use gsb_core::room::{Action, Admission, GameLogic, RoomLogic, TickCtx};
use gsb_protocol::{MessageTable, base, op};
use gsb_server::{Config, GameModule, RegistryParts, RegistryTask, ServerError, ServerHooks};
use prost::Message;
use tokio::sync::mpsc;

const W: Duration = Duration::from_secs(10);
const REASON: &str = "speed hack";

/// A room logic that kicks every player whose game input it ingests.
struct KickOnInput;

impl GameLogic<World> for KickOnInput {
    type GroupKey = ();
    type Strip = ();
    fn snapshot_op(&self) -> u16 {
        gsb_demo::op::WORLD_SNAPSHOT
    }
    fn private_op(&self) -> u16 {
        gsb_demo::op::PRIVATE
    }
    fn group_of(&self, _w: &World, _p: PlayerId) {}
    fn snapshot(
        &mut self,
        _w: &mut World,
        _ctx: &TickCtx,
        _g: &(),
        _borrowed: &[gsb_core::shard::BorderRecord<()>],
        _out: &mut bytes::BytesMut,
    ) -> bool {
        false
    }
    fn on_join(&mut self, _w: &mut World, conn: ConnectionId) -> Admission {
        Admission {
            player: PlayerId(conn.0),
            entity: conn.0,
        }
    }
    fn on_leave(&mut self, _w: &mut World, _p: PlayerId) {}
    fn ingest(&mut self, _w: &mut World, ctx: &TickCtx, actions: &mut Vec<Action>) {
        for a in actions.drain(..) {
            ctx.kick(a.player, REASON);
        }
    }
    fn update(&mut self, _w: &mut World, _ctx: &TickCtx) {}
}

impl RoomLogic<World> for KickOnInput {}

/// The module hosting [`KickOnInput`] rooms (the demo's wire table, so
/// its `MOVE_TO` is a registered game-band opcode).
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
            logic: Box::new(KickOnInput) as Box<dyn RoomLogic<World, GroupKey = (), Strip = ()>>,
        });
        parts.spawn(factory)
    }
    fn describe(&self) -> String {
        "kicking".into()
    }
}

async fn player(addr: std::net::SocketAddr, name: &str) -> Conn {
    let mut c = gsb_client::connect::tcp(addr).await.expect("connect");
    session::auth_and_join(&mut c, &Credentials::named(name), 1, W, |_| {})
        .await
        .unwrap_or_else(|e| panic!("{name}: join failed: {e}"));
    c
}

/// Every ERROR frame until the server closes the connection (in the
/// order read — an ERROR read here came BEFORE the close).
async fn errors_until_close(c: &mut Conn) -> Vec<base::Error> {
    let mut errors = Vec::new();
    let end = Instant::now() + W;
    loop {
        let left = end
            .checked_duration_since(Instant::now())
            .expect("the server never closed the connection");
        match c.recv(left).await {
            Ok(Recv::Frame(f)) if f.op == op::base::ERROR => {
                errors.push(base::Error::decode(&f.payload[..]).expect("ERROR decodes"));
            }
            Ok(Recv::Frame(_)) | Ok(Recv::Quiet) => {}
            Ok(Recv::Closed) | Err(_) => return errors,
        }
    }
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

/// A heartbeat of the bystander's is answered (the connection actor
/// answers at most one a second, so a probe may go unanswered once:
/// retry until one of THESE is acknowledged).
async fn still_answered(c: &mut Conn) {
    const PROBE: u64 = 1000;
    let deadline = Instant::now() + W;
    let mut tick = PROBE;
    loop {
        match session::heartbeat_round(c, tick, Duration::from_millis(300), |_| {}).await {
            Ok(acked) if acked >= PROBE => return,
            Ok(_) | Err(ClientError::TimedOut) => {
                assert!(Instant::now() < deadline, "no heartbeat answered in {W:?}");
                tick += 1;
            }
            Err(e) => panic!("the bystander was dropped: {e}"),
        }
    }
}

#[tokio::test]
async fn a_kicked_client_reads_error_9_with_the_reason_then_the_close() {
    let (tx, mut reports) = mpsc::unbounded_channel();
    let cfg = Config {
        bind: "127.0.0.1:0".into(),
        room_count: 1,
        game: "kicking".into(),
        ..Config::default()
    };
    let handle = gsb_server::start_game_server_with(
        Box::new(Kicking),
        cfg,
        ServerHooks::default(),
        Some(tx),
    )
    .await
    .expect("server starts");
    let mut kicked = player(handle.addr, "cheat").await;
    let mut bystander = player(handle.addr, "honest").await;
    let input = gsb_demo::game::MoveTo { x: 1, y: 1, seq: 1 }.encode_to_vec();
    kicked
        .send(gsb_demo::op::MOVE_TO, &input)
        .await
        .expect("send");
    let errors = errors_until_close(&mut kicked).await;
    assert_eq!(errors.len(), 1, "one notice, then the close: {errors:?}");
    assert_eq!(errors[0].code(), base::ErrorCode::ServerClosed);
    assert_eq!(errors[0].message, format!("kicked: {REASON}"));
    let r = closes(&mut reports, 1).await;
    for (reason, n) in r.net.server_closes.iter() {
        assert_eq!(n, u64::from(reason == ServerClose::Kicked), "{reason:?}");
    }
    still_answered(&mut bystander).await;
    handle.stop().await;
}
