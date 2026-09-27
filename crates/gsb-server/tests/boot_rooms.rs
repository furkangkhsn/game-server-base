//! The boot rooms (`room_count`) exist before any join can reach the
//! registry (BACKLOG B44).
//!
//! The race it pins: startup used to send each boot room's `CreateRoom`
//! from a spawned task and open the doors without waiting, so a client
//! that connected and joined at once could reach the registry first and
//! be told `room 1 not found` (seen under load in the loadgen runs).
//!
//! Deterministic without any scheduling luck: the test runs on the
//! current-thread runtime and asks the registry about the boot rooms
//! right after `start` returns, before the test task ever yields — so no
//! spawned task has run yet. The registry drains its mailbox in order,
//! so the answer is "running" exactly when every boot `CreateRoom` was
//! already in the mailbox when `start` returned; a create still waiting
//! in a spawned task answers `Absent`. A join from an accept loop is a
//! later message on the same mailbox, so it is behind the creates too.

use gsb_core::id::RoomId;
use gsb_core::registry::RoomStatus;
use gsb_server::Config;

const ROOMS: u64 = 3;

fn cfg() -> Config {
    Config {
        bind: "127.0.0.1:0".into(),
        room_count: ROOMS,
        ..Default::default()
    }
}

#[tokio::test(flavor = "current_thread")]
async fn every_boot_room_is_ahead_of_the_first_request() {
    let handle = gsb_server::start_server(cfg())
        .await
        .expect("server starts");
    for id in 1..=ROOMS {
        let status = handle.room_status(RoomId(id)).await.expect("registry");
        assert_eq!(
            status,
            RoomStatus::Running { members: 0 },
            "boot room {id} was not created before the first request reached the registry"
        );
    }
    handle.stop().await;
}
