//! The path signal end to end (BACKLOG B103): an rUDP door with the
//! congestion response on, a reporting client behind a bottleneck, and a
//! kit `OpenRoom` opted in to `SnapshotBudget`. The writer paces the
//! session and tells its connection actor; the actor tells the room; the
//! game reads `TickCtx::budget` = `Some`; and the room withholds the
//! frames the path cannot carry (`snapshots_withheld > 0`).
//!
//! Real sockets and the rUDP client's wall clock: every wait is a
//! condition with a hang guard.

#![cfg(feature = "game-demo")]

#[path = "path_budget/game.rs"]
mod game;
#[path = "path_budget/relay.rs"]
mod relay;

use std::time::{Duration, Instant};

use gsb_client::session::{self, Credentials};
use gsb_client::{Recv, ServerError};
use gsb_core::metrics::MetricReport;
use gsb_protocol::op::base as op;
use tokio::sync::mpsc;

use game::SwarmModule;
use relay::{GUARD, Relay};

/// The bottleneck: 8 kB/s, 3 kB deep — the room offers ~45 kB/s.
const RATE: f64 = 8_000.0;
const BURST: f64 = 3_000.0;

/// The smallest swarm frame: 41 records (the dots and the player) of 32
/// bytes, before the envelope.
const FRAME_FLOOR: usize = 41 * 32;

fn config(congestion: &str) -> gsb_server::Config {
    let dir = std::env::temp_dir().join(format!(
        "gsb-path-budget-{}-{congestion}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let file = dir.join("server.toml");
    std::fs::write(
        &file,
        format!(
            "bind = \"127.0.0.1:0\"\ntransport = \"udp\"\nroom_count = 1\n\
             udp_congestion = \"{congestion}\"\n"
        ),
    )
    .expect("write the config");
    gsb_server::Config::from_file(&file).expect("the config parses")
}

/// One run: a client behind the bottleneck reads for `secs`; returns the
/// budgets the game read and room 1's `snapshots_withheld`, as the last
/// metric report folded before the end had them.
async fn run(congestion: &str, secs: u64) -> (Vec<usize>, u64) {
    let (budgets_tx, mut budgets) = mpsc::unbounded_channel();
    let (reports_tx, mut reports) = mpsc::unbounded_channel::<MetricReport>();
    let handle = gsb_server::start_game_server_with(
        Box::new(SwarmModule {
            budgets: budgets_tx,
        }),
        config(congestion),
        Default::default(),
        Some(reports_tx),
    )
    .await
    .expect("the server starts");
    let relay = Relay::start(handle.addr, RATE, BURST).await;
    let mut conn = gsb_client::connect::udp(relay.addr)
        .await
        .expect("rUDP handshake through the relay");
    session::auth_and_join(&mut conn, &Credentials::named("dot"), 1, GUARD, |_| {})
        .await
        .expect("join");
    let end = Instant::now() + Duration::from_secs(secs);
    let mut tick = 0;
    let mut warm = Instant::now();
    while Instant::now() < end {
        if Instant::now() >= warm {
            tick += 1;
            let hb = session::heartbeat(tick);
            conn.send(hb.op, &hb.payload).await.expect("heartbeat");
            warm = Instant::now() + Duration::from_millis(300);
        }
        match conn.recv(Duration::from_millis(50)).await {
            Ok(Recv::Frame(f)) if f.op == op::ERROR => {
                panic!("{}", ServerError::decode_lossy(&f.payload))
            }
            Ok(Recv::Closed) => panic!("the stream ended"),
            Ok(_) => {}
            Err(e) => panic!("{e}"),
        }
    }
    let mut withheld = 0;
    while let Ok(r) = reports.try_recv() {
        for room in r.rooms.iter().filter(|r| r.room.0 == 1) {
            withheld = room.snapshots_withheld;
        }
    }
    let mut seen = Vec::new();
    while let Ok(b) = budgets.try_recv() {
        seen.push(b);
    }
    relay.stop();
    handle.stop().await;
    (seen, withheld)
}

#[tokio::test]
async fn a_paced_rudp_session_s_budget_reaches_the_game_and_the_kit_thins() {
    let (budgets, withheld) = run("pace", 8).await;
    assert!(
        !budgets.is_empty(),
        "the game read TickCtx::budget = Some for its member"
    );
    assert!(
        budgets.iter().all(|b| *b < FRAME_FLOOR),
        "the path cannot take a whole frame a tick: {budgets:?}"
    );
    assert!(withheld > 0, "the kit withheld frames: {withheld}");
}

/// The same run with the response off: the room never learns a budget
/// and ships every frame (the policer drops what it drops).
#[tokio::test]
async fn with_the_response_off_the_room_knows_no_budget() {
    let (budgets, withheld) = run("off", 5).await;
    assert_eq!(budgets, Vec::<usize>::new());
    assert_eq!(withheld, 0);
}
