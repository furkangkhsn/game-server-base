//! `POST /rooms/open` opens the SERVER's room (BACKLOG F8, `docs/OPS.md`
//! §2): the room-level keys every pre-created room gets reach a room
//! opened at runtime too, with `tick_hz` the one per-request override.

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use gsb_core::id::RoomId;
use gsb_core::registry::RoomStatus;
use gsb_server::Config;

/// One raw ops request over a fresh connection: (status, full text).
async fn request(addr: SocketAddr, head: &str) -> (u16, String) {
    let mut s = TcpStream::connect(addr).await.expect("ops listener");
    s.write_all(head.as_bytes()).await.expect("request");
    let mut buf = Vec::new();
    s.read_to_end(&mut buf).await.expect("response");
    let text = String::from_utf8_lossy(&buf).into_owned();
    let status = text
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| panic!("no status line: {text:?}"));
    (status, text)
}

/// A server whose room-level keys are all off their defaults: the room
/// every creation path must reproduce (BACKLOG F8).
fn tuned_config() -> Config {
    Config {
        bind: "127.0.0.1:0".into(),
        room_count: 1,
        http_listen: "127.0.0.1:0".into(),
        room_control: 64,
        conn_action: 128,
        max_snapshot_bytes: 1200,
        keepalive_hz: 2.0,
        max_players: Some(12),
        max_idle_input_secs: Some(40),
        max_detach_hold: Some(Duration::from_secs(3)),
        ..Default::default()
    }
}

/// The admin open builds the SERVER's room (F8): on a server whose
/// room-level keys are all non-default, re-opening the pre-created room
/// is the idempotent no-op (the request equals the boot room's config —
/// it used to be a 409, the surface built `RoomConfig::default()`); an
/// explicit `tick_hz` equal to the server's rate is the same request; a
/// different rate for the live room stays a conflict; a new room opens
/// and re-opens idempotently; an invalid rate is still refused.
#[tokio::test]
async fn admin_open_uses_the_server_room_config() {
    let handle = gsb_server::start_server(tuned_config())
        .await
        .expect("server starts");
    let addr = handle.http_addr.expect("ops addr");
    // The boot room exists before the admin request races its creation.
    let deadline = Instant::now() + Duration::from_secs(5);
    while handle.room_status(RoomId(1)).await.expect("registry") == RoomStatus::Absent {
        assert!(Instant::now() < deadline, "the boot room never came up");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    for head in [
        "POST /rooms/open?id=1 HTTP/1.1\r\n\r\n",
        "POST /rooms/open?id=1&tick_hz=30 HTTP/1.1\r\n\r\n",
    ] {
        let (status, text) = request(addr, head).await;
        assert_eq!(status, 200, "{head:?} re-opens the boot room: {text}");
        assert!(text.contains("r1 running"), "{text}");
    }
    let (status, text) = request(addr, "POST /rooms/open?id=1&tick_hz=15 HTTP/1.1\r\n\r\n").await;
    assert_eq!(status, 409, "another rate for a live room: {text}");

    for _ in 0..2 {
        let (status, text) = request(addr, "POST /rooms/open?id=7 HTTP/1.1\r\n\r\n").await;
        assert_eq!(status, 200, "open and idempotent re-open: {text}");
        assert!(text.contains("r7 running"), "{text}");
    }
    for bad in ["0", "-3", "abc", "7"] {
        let head = format!("POST /rooms/open?id=8&tick_hz={bad} HTTP/1.1\r\n\r\n");
        let (status, text) = request(addr, &head).await;
        assert_eq!(status, 400, "tick_hz={bad} is refused: {text}");
    }
    handle.stop().await;
}
