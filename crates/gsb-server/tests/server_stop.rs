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

use gsb_protocol::base::{Auth, JoinRoom};
use gsb_server::{Config, Topology};
use prost::Message;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

const MEMBERS: usize = 24;
const STOP_WITHIN: Duration = Duration::from_secs(10);

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

/// One length-prefixed frame (`[u32 LE len][u16 LE op][payload]`).
fn frame(op: u16, payload: &[u8]) -> Vec<u8> {
    let mut out = ((2 + payload.len()) as u32).to_le_bytes().to_vec();
    out.extend_from_slice(&op.to_le_bytes());
    out.extend_from_slice(payload);
    out
}

/// Connect, authenticate as `name`, join room 1, and return the socket
/// once the join result arrived.
async fn join(addr: std::net::SocketAddr, name: String) -> TcpStream {
    let mut s = TcpStream::connect(addr).await.expect("connect");
    let auth = Auth {
        name,
        ticket: Vec::new(),
        protocol_version: gsb_protocol::PROTOCOL_VERSION,
    };
    let mut hello = frame(gsb_protocol::op::base::AUTH_REQ, &auth.encode_to_vec());
    let join = JoinRoom { room_id: 1 }.encode_to_vec();
    hello.extend(frame(gsb_protocol::op::base::JOIN_ROOM_REQ, &join));
    s.write_all(&hello).await.expect("write");
    loop {
        let len = s
            .read_u32_le()
            .await
            .expect("the server closed before the join") as usize;
        let mut body = vec![0u8; len];
        s.read_exact(&mut body).await.expect("frame body");
        let op = u16::from_le_bytes([body[0], body[1]]);
        assert_ne!(op, gsb_protocol::op::base::ERROR, "join refused");
        if op == gsb_protocol::op::base::JOIN_ROOM_RESULT {
            return s;
        }
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
