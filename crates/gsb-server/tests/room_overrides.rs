//! Per-room overrides (`[rooms.<id>]`, BACKLOG B18) against the real
//! server: the file form loads, a room the registry would refuse refuses
//! startup, the admin open builds the id's room (the query `tick_hz` on
//! top) and re-opens it idempotently, and a room's own `max_players`
//! refuses the join past it while the server's other room accepts.

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use gsb_client::session::{self, Credentials};
use gsb_core::error::CoreError;
use gsb_core::id::RoomId;
use gsb_core::registry::RoomStatus;
use gsb_protocol::base::ErrorCode;
use gsb_server::{Config, ConfigError, RoomOverride, ServerError};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

const W: Duration = Duration::from_secs(10);

/// `text` written to a temp file and loaded as the server loads it.
fn load(tag: &str, text: &str) -> Result<Config, ConfigError> {
    let path = std::env::temp_dir().join(format!(
        "gsb-room-overrides-{tag}-{}.toml",
        std::process::id()
    ));
    std::fs::write(&path, text).expect("write temp config");
    let cfg = Config::from_file(&path);
    let _ = std::fs::remove_file(&path);
    cfg
}

/// A local server with `rooms` overridden.
fn config(room_count: u64, rooms: &[(u64, RoomOverride)]) -> Config {
    Config {
        bind: "127.0.0.1:0".into(),
        http_listen: "127.0.0.1:0".into(),
        room_count,
        rooms: rooms.iter().cloned().collect(),
        ..Config::default()
    }
}

/// The file form: a section reaches its room, the rest keep the server's
/// values; a key that is not room-level fails the load, named.
#[test]
fn the_file_form_loads_and_refuses_a_stray_key() {
    let cfg = load(
        "ok",
        "max_players = 50\nroom_count = 2\n[rooms.1]\nmax_players = 2\n",
    )
    .expect("loads");
    assert_eq!(cfg.room_config(1).max_players, Some(2));
    assert_eq!(cfg.room_config(2).max_players, Some(50));
    assert!(cfg.raw.contains_key("rooms"), "the raw table keeps it");

    let e = load(
        "stray",
        "[rooms.1]\nmax_players = 2\nbind = \"0.0.0.0:1\"\n",
    )
    .expect_err("refused");
    assert!(matches!(e, ConfigError::Parse { .. }), "{e:?}");
    let text = format!("{e}: {}", std::error::Error::source(&e).expect("source"));
    assert!(text.contains("unknown field `bind`"), "{text}");
}

/// A room whose rate does not divide the global one refuses startup
/// (at boot it would only have logged a warning).
#[tokio::test]
async fn a_room_the_registry_would_refuse_refuses_startup() {
    let bad = RoomOverride {
        tick_hz: Some(7.0),
        ..RoomOverride::default()
    };
    let Err(e) = gsb_server::start_server(config(2, &[(2, bad)])).await else {
        panic!("a tick_hz of 7 under 30 started");
    };
    assert!(
        matches!(
            e,
            ServerError::RoomOverride {
                id: 2,
                source: CoreError::TickRate { .. }
            }
        ),
        "{e:?}"
    );
}

/// One raw ops request over a fresh connection: the status code.
async fn post(addr: SocketAddr, target: &str) -> u16 {
    let mut s = TcpStream::connect(addr).await.expect("ops listener");
    let head = format!("POST {target} HTTP/1.1\r\n\r\n");
    s.write_all(head.as_bytes()).await.expect("request");
    let mut buf = Vec::new();
    s.read_to_end(&mut buf).await.expect("response");
    let text = String::from_utf8_lossy(&buf).into_owned();
    text.split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| panic!("no status line: {text:?}"))
}

/// Wait until the boot room `id` exists.
async fn boot_room(handle: &gsb_server::ServerHandle, id: u64) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while handle.room_status(RoomId(id)).await.expect("registry") == RoomStatus::Absent {
        assert!(Instant::now() < deadline, "boot room {id} never came up");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// The admin open builds the id's room: re-opening the overridden boot
/// room is idempotent at ITS rate and a conflict at the server's; a
/// runtime id opens with its override (the registry holds exactly
/// `room_config(7)`), re-opens idempotently, and takes the query rate
/// on top.
#[tokio::test]
async fn the_admin_open_builds_the_overridden_room() {
    let one = RoomOverride {
        tick_hz: Some(15.0),
        max_players: Some(2),
        ..RoomOverride::default()
    };
    let seven = RoomOverride {
        tick_hz: Some(10.0),
        max_players: Some(3),
        ..RoomOverride::default()
    };
    let cfg = config(1, &[(1, one), (7, seven)]);
    let handle = gsb_server::start_server(cfg.clone())
        .await
        .expect("server starts");
    let addr = handle.http_addr.expect("ops addr");
    boot_room(&handle, 1).await;

    assert_eq!(post(addr, "/rooms/open?id=1").await, 200, "its own room");
    assert_eq!(post(addr, "/rooms/open?id=1&tick_hz=15").await, 200);
    assert_eq!(
        post(addr, "/rooms/open?id=1&tick_hz=30").await,
        409,
        "the server's rate is not room 1's"
    );

    for _ in 0..2 {
        assert_eq!(post(addr, "/rooms/open?id=7").await, 200);
    }
    let status = handle.open_room(cfg.room_config(7)).await;
    assert!(
        matches!(status, Ok(RoomStatus::Running { .. })),
        "the runtime room IS room_config(7): {status:?}"
    );
    assert_eq!(post(addr, "/rooms/open?id=7&tick_hz=10").await, 200);
    assert_eq!(post(addr, "/rooms/open?id=7&tick_hz=15").await, 409);
    assert_eq!(
        post(addr, "/rooms/open?id=8&tick_hz=15").await,
        200,
        "the query rate on a room without an override"
    );
    handle.stop().await;
}

/// End to end: room 1's own `max_players = 2` answers the third joiner
/// with the existing RoomFull error (its connection stays usable),
/// while room 2 — the server's room — takes it.
#[tokio::test]
async fn a_room_cap_of_its_own_refuses_the_third_join() {
    let cap = RoomOverride {
        max_players: Some(2),
        ..RoomOverride::default()
    };
    let handle = gsb_server::start_server(config(2, &[(1, cap)]))
        .await
        .expect("server starts");
    boot_room(&handle, 1).await;
    boot_room(&handle, 2).await;

    let mut members = Vec::new();
    for name in ["cap-a", "cap-b"] {
        let mut c = gsb_client::connect::tcp(handle.addr).await.expect("tcp");
        session::auth_and_join(&mut c, &Credentials::named(name), 1, W, |_| {})
            .await
            .unwrap_or_else(|e| panic!("{name} joins room 1: {e}"));
        members.push(c);
    }

    let mut third = gsb_client::connect::tcp(handle.addr).await.expect("tcp");
    let e = session::auth_and_join(&mut third, &Credentials::named("cap-c"), 1, W, |_| {})
        .await
        .expect_err("room 1 is full");
    let s = e
        .server()
        .unwrap_or_else(|| panic!("not a server error: {e}"));
    assert_eq!((s.code, s.raw), (ErrorCode::RoomFull, 8), "{s}");

    session::join(&mut third, 2, W, |_| {})
        .await
        .expect("room 2 (the server's cap) takes the same connection");
    handle.stop().await;
}
