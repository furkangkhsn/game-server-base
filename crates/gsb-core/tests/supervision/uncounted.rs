//! A panicked room or shard sends no final sample (B67): its death
//! watcher reports the task as ended without its final count, under the
//! task's row id, so the collector counts the loss and lets the row go
//! (no ghost). A sharded room's SURVIVING shards stop with the reaped
//! room and send their own final counts (before, they ran on).

use super::*;

use gsb_core::metrics::MetricsEvent;

use super::counters::start_observed;

/// Read metrics events until `want` has seen what it waits for (it
/// returns `true`), within the wait.
async fn watch(
    metrics: &mut mpsc::Receiver<MetricsEvent>,
    mut want: impl FnMut(&MetricsEvent) -> bool,
) {
    let deadline = tokio::time::Instant::now() + WAIT;
    loop {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        match tokio::time::timeout(left, metrics.recv()).await {
            Ok(Some(ev)) => {
                if want(&ev) {
                    return;
                }
            }
            other => panic!("the awaited metrics never came: {other:?}"),
        }
    }
}

/// A single room whose logic panics: the watcher reports it under the
/// room's own id, and no final sample of it ever arrives.
#[tokio::test]
async fn a_panicked_room_is_reported_as_ended_uncounted() {
    let (tx, mut metrics, handle) = start_observed(Arc::new(|_id, _cfg| BuiltRoom::Single {
        world: (),
        logic: Box::new(PanicAfterJoinLogic {
            joined: false,
            armed: true,
        }) as Box<dyn RoomLogic<(), GroupKey = (), Strip = ()>>,
    }));
    create_with(&tx, config(RoomId(1), false))
        .await
        .expect("room created");
    let _inbox = open_conn(&tx, ConnectionId(1)).await;
    let _ = spawn(&tx, ConnectionId(1), RoomId(1)).await;
    let mut finals = 0;
    watch(&mut metrics, |ev| match ev {
        MetricsEvent::RoomFinal(_) => {
            finals += 1;
            false
        }
        MetricsEvent::RoomEndedUncounted(row) => {
            assert_eq!(*row, RoomId(1), "the room's own row");
            true
        }
        _ => false,
    })
    .await;
    assert_eq!(finals, 0, "a panicked room sends no final sample");
    stop(tx, handle).await;
}

/// One shard of two panics: its row is reported ended uncounted, and
/// the surviving shard stops with the reaped room — its final count
/// arrives under its own row.
#[tokio::test]
async fn a_panicked_shard_is_reported_and_its_survivor_stops_counted() {
    let factory: RoomFactory<(), (), (), ()> = Arc::new(|_id, _config| BuiltRoom::Sharded {
        shards: vec![
            (
                (),
                Box::new(TimeBombShardLogic {
                    index: 0,
                    detonate_at: None,
                })
                    as Box<dyn ShardLogic<(), GroupKey = (), State = (), Strip = ()>>,
            ),
            (
                (),
                Box::new(TimeBombShardLogic {
                    index: 1,
                    detonate_at: Some(Instant::now() + Duration::from_millis(200)),
                })
                    as Box<dyn ShardLogic<(), GroupKey = (), State = (), Strip = ()>>,
            ),
        ],
        home_shard: Arc::new(|_conn, _identity: &str| 0),
    });
    let (tx, mut metrics, handle) = start_observed(factory);
    let id = RoomId(23);
    create_with(&tx, config(id, false))
        .await
        .expect("create failed");
    let (dead, survivor) = (RoomId((23 << 16) | 1), RoomId(23 << 16));
    let (mut reported, mut survivor_final) = (false, false);
    watch(&mut metrics, |ev| {
        match ev {
            MetricsEvent::RoomEndedUncounted(row) => {
                assert_eq!(*row, dead, "the dead shard's row");
                reported = true;
            }
            MetricsEvent::RoomFinal(s) => {
                assert_ne!(s.room, dead, "the dead shard has no final count");
                if s.room == survivor {
                    survivor_final = true;
                }
            }
            _ => {}
        }
        reported && survivor_final
    })
    .await;
    stop(tx, handle).await;
}
