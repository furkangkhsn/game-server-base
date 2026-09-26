//! `ServerHandle::stop` always completes (BACKLOG §1 row 4a), end to end.
//!
//! The hang it pins: members disconnect just before the stop, and their
//! routed DETACHes overfill a room's control channel (`room_control`).
//! `stop()` aborts the ticker right after enqueueing the registry's
//! `Shutdown`, so the room never drains that channel again; a registry
//! that awaited a bounded send of the room's `Shutdown` into it waited
//! forever, holding the `Ticker` that keeps the broadcast open — and
//! `stop()` waits on the metrics collector, which waits on that broadcast.
//!
//! Deterministic without any scheduling luck: the tick is slow (10 Hz)
//! and the channel tiny (2), so the 24 detaches need ~12 ticks (> 1 s) to
//! drain, while `stop()` runs ~150 ms after the burst — the channel is
//! still full, with detaches parked on it, whichever tick lands between.
//! The gsb-core suite (`tests/shutdown.rs`) pins the same state directly
//! at the registry.

use std::time::Duration;

use gsb_client::session::{self, Credentials};
use gsb_client::{ClientError, Conn};
use gsb_server::{Config, Topology};

const MEMBERS: usize = 24;
const STOP_WITHIN: Duration = Duration::from_secs(10);
const JOIN_WITHIN: Duration = Duration::from_secs(10);

fn cfg(topology: Option<Topology>) -> Config {
    Config {
        bind: "127.0.0.1:0".into(),
        room_count: 1,
        tick_hz: 10.0,
        room_control: 2,
        idle_timeout_secs: 0.0,
        topology,
        ..Default::default()
    }
}

/// Connect, authenticate as `name`, join room 1, and return the
/// connection once the join result arrived.
async fn join(addr: std::net::SocketAddr, name: String) -> Conn {
    let mut c = gsb_client::connect::tcp(addr).await.expect("connect");
    match session::auth_and_join(&mut c, &Credentials::named(name), 1, JOIN_WITHIN, |_| {}).await {
        Ok(_) => c,
        Err(e @ (ClientError::Server(_) | ClientError::AuthRefused(_))) => {
            panic!("join refused: {e}")
        }
        Err(e) => panic!("the server closed before the join: {e}"),
    }
}

async fn stop_after_a_disconnect_burst(topology: Option<Topology>, tag: &str) {
    let handle = gsb_server::start_server(cfg(topology))
        .await
        .expect("server starts");
    let joins: Vec<_> = (0..MEMBERS)
        .map(|i| tokio::spawn(join(handle.addr, format!("{tag}-{i}"))))
        .collect();
    let mut sockets = Vec::with_capacity(MEMBERS);
    for j in joins {
        sockets.push(j.await.expect("join task"));
    }
    // Every member's transport dies at once; the detaches pile up in the
    // room's (or each shard's) control channel.
    drop(sockets);
    tokio::time::sleep(Duration::from_millis(150)).await;
    tokio::time::timeout(STOP_WITHIN, handle.stop())
        .await
        .expect("stop() hung: the registry is parked on a full room control channel");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stop_completes_after_a_disconnect_burst_single_room() {
    stop_after_a_disconnect_burst(None, "single").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stop_completes_after_a_disconnect_burst_sharded_room() {
    stop_after_a_disconnect_burst(Some(Topology::Sharded), "sharded").await;
}
