//! A runtime destroy of a SHARDED room, with the ticker still running:
//! every shard must stop. Unlike the whole-server stop there is no
//! backstop here — the broadcast stays open, so a shard that never gets
//! its `ShardMsg::Shutdown` lives (and ticks) forever. Each shard's
//! teardown is observed through its match result (one per shard).

use std::time::Duration;

use tokio::sync::oneshot;

use gsb_core::id::ConnectionId;
use gsb_core::registry::{RegistryMsg, RoomStatus};

use super::{
    HZ, MEMBERS, ROOM, Rig, ask, expect_stopped, populate, populate_at, sharded_room, start,
};

/// A slow room rate for the full-mailbox case: a shard steps every
/// 250 ms and admits `CAP` (2) control messages per step, so a backlog of
/// `MEMBERS` detaches per shard stays queued for ~1.5 s.
const SLOW_HZ: f64 = 4.0;
/// Enough for the backlog to drain and the Shutdown to land behind it.
const TEARDOWN_WAIT: Duration = Duration::from_secs(10);

/// Destroy and require the table to drop the room at once.
async fn destroy(rig: &Rig) {
    let (reply, rx) = oneshot::channel();
    let status = ask(&rig.tx, RegistryMsg::DestroyRoom { id: ROOM, reply }, rx).await;
    assert_eq!(status, RoomStatus::Destroyed);
}

/// Every shard ran its teardown while the ticker kept running, and no
/// extra teardown shows up (one result per shard, exactly).
async fn expect_every_shard_stopped(rig: &mut Rig) {
    for shard in 0..2 {
        let result = tokio::time::timeout(TEARDOWN_WAIT, rig.results.recv())
            .await
            .unwrap_or_else(|_| panic!("shard #{shard} never stopped after the destroy"))
            .expect("result sink closed early");
        assert_eq!(result.room, ROOM);
    }
    assert!(
        rig.results.try_recv().is_err(),
        "more teardowns than shards"
    );
}

/// Then the server stop, in `ServerHandle::stop`'s order (registry
/// `Shutdown`, ticker abort): nothing is left to tear down.
async fn stop_like_the_server(rig: Rig) {
    rig.tx
        .send(RegistryMsg::Shutdown)
        .await
        .expect("registry gone");
    rig.ticker_task.abort();
    expect_stopped(rig, 0).await;
}

/// The plain case: room in the mailboxes, members live, ticker running.
#[tokio::test]
async fn destroying_a_sharded_room_stops_every_shard() {
    let mut rig = start(sharded_room());
    populate(&mut rig).await;
    destroy(&rig).await;
    expect_every_shard_stopped(&mut rig).await;
    stop_like_the_server(rig).await;
}

/// The full-mailbox case: every member's transport dies, the routed
/// detaches overfill each shard's capacity-2 mailbox, and the destroy
/// lands while they are still queued. The registry answers at once (its
/// Shutdown goes to a spawned sender), and once the shards drain their
/// backlog on their own ticks, that sender delivers — every shard stops.
#[tokio::test]
async fn destroy_reaches_shards_whose_mailboxes_are_full() {
    assert_eq!(
        HZ % SLOW_HZ,
        0.0,
        "the room rate must divide the global rate"
    );
    let mut rig = start(sharded_room());
    populate_at(&mut rig, SLOW_HZ).await;
    for c in 1..=MEMBERS {
        let msg = RegistryMsg::ConnClosed {
            conn: ConnectionId(c),
            verdict: None,
        };
        rig.tx.send(msg).await.expect("registry gone");
    }
    // Let the dispatchers fill both mailboxes and park on them (at most
    // one slow step can land in this window; the backlog outlives it).
    tokio::time::sleep(Duration::from_millis(50)).await;
    destroy(&rig).await;
    expect_every_shard_stopped(&mut rig).await;
    stop_like_the_server(rig).await;
}
