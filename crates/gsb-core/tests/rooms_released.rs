//! The rooms' drop barrier (BACKLOG F5): `Registry::with_rooms_hold` makes
//! the waiter complete only once the registry is gone AND every room and
//! shard task it spawned has run its teardown — the moment a server may
//! stop its game services without cutting off the rooms' last words.
//!
//! Each room's `on_shutdown` is deliberately slow (a blocking sleep on a
//! multi-thread runtime), so a barrier that released on the registry's exit
//! alone — or on a room's END being reported instead of its task's end —
//! would complete before the teardowns land.

use std::sync::Arc;
use std::time::Duration;

use gsb_core::channel::channel;
use gsb_core::id::RoomId;
use gsb_core::registry::{BuiltRoom, MatchResult, Registry, RegistryMsg, RoomFactory};
use gsb_core::room::RoomConfig;
use gsb_core::service::hold;
use gsb_core::ticker::Ticker;
use tokio::sync::{mpsc, oneshot};

#[path = "rooms_released/logic.rs"]
mod logic;
use logic::{Slow, SlowShard, Torn};

const WAIT: Duration = Duration::from_secs(5);

/// Two rooms: single rooms, or two-shard grids when `sharded`.
fn factory(torn: mpsc::Sender<Torn>, sharded: bool) -> RoomFactory<(), (), (), ()> {
    Arc::new(move |_id, _config| {
        if sharded {
            let shard = |index| logic::shard(SlowShard::new(index, torn.clone()));
            BuiltRoom::Sharded {
                shards: vec![shard(0), shard(1)],
                home_shard: Arc::new(|_conn, _identity: &str| 0),
            }
        } else {
            logic::single(Slow::new(torn.clone()))
        }
    })
}

/// Start a registry over `factory` with the barrier, create two rooms,
/// stop it the way `ServerHandle::stop` does, wait for the barrier and
/// return every teardown that had landed by then (sorted).
async fn teardowns_seen_at_release(sharded: bool) -> Vec<Torn> {
    let (torn_tx, mut torn) = mpsc::channel::<Torn>(16);
    let (tx, rx) = channel::<RegistryMsg>(64);
    let (ticker, ticker_task) = Ticker::spawn(60.0, 64).expect("valid tick rate");
    let (metrics_tx, _metrics) = mpsc::channel::<gsb_core::metrics::MetricsEvent>(64);
    let (result_tx, _results) = channel::<MatchResult>(16);
    let (rooms_hold, rooms_released) = hold();
    let registry = tokio::spawn(
        Registry::new(
            rx,
            tx.clone(),
            factory(torn_tx, sharded),
            ticker,
            metrics_tx,
            None,
            None,
            Some(result_tx),
        )
        .with_rooms_hold(rooms_hold)
        .run(),
    );
    for id in 1..=2 {
        let config = RoomConfig {
            id: RoomId(id),
            ..Default::default()
        };
        let (reply, answer) = oneshot::channel();
        tx.send(RegistryMsg::CreateRoom { config, reply })
            .await
            .expect("registry gone");
        answer.await.expect("reply dropped").expect("create failed");
    }
    // `ServerHandle::stop`'s order: the registry's Shutdown, the clock.
    tx.send(RegistryMsg::Shutdown).await.expect("registry gone");
    ticker_task.abort();
    tokio::time::timeout(WAIT, rooms_released.wait())
        .await
        .expect("the barrier never released");
    assert!(registry.is_finished(), "released while the registry ran");
    let mut seen = Vec::new();
    while let Ok(t) = torn.try_recv() {
        seen.push(t);
    }
    seen.sort();
    seen
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_barrier_waits_for_every_room_teardown() {
    assert_eq!(
        teardowns_seen_at_release(false).await,
        vec![Torn::Room, Torn::Room],
        "released before every room had run its teardown"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_barrier_waits_for_every_shard_teardown() {
    assert_eq!(
        teardowns_seen_at_release(true).await,
        vec![
            Torn::Shard(0),
            Torn::Shard(0),
            Torn::Shard(1),
            Torn::Shard(1)
        ],
        "released before every shard had run its teardown"
    );
}
