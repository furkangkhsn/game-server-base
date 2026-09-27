//! A match result the sink refused is counted (BACKLOG B57): the room
//! tells the metrics collector why — a FULL sink (the consumer is not
//! reading) or a CLOSED one (the consumer dropped its receiver) — since
//! a stopping room sends no further sample of its own.

use super::*;
use gsb_core::metrics::{MatchResultDrop, MetricsEvent};

/// A registry over rooms that each report a result, with `sink` as the
/// result sink; the metrics receiver is kept.
fn start_observed(
    sink: Mailbox<MatchResult>,
) -> (
    Mailbox<RegistryMsg>,
    mpsc::Receiver<MetricsEvent>,
    tokio::task::JoinHandle<()>,
) {
    let factory: RoomFactory<(), (), (), ()> = Arc::new(|_id, _config| BuiltRoom::Single {
        world: (),
        logic: Box::new(ResultLogic {
            result: Some(vec![1]),
        }) as Box<dyn RoomLogic<(), GroupKey = (), Strip = ()>>,
    });
    let (tx, rx) = channel::<RegistryMsg>(4096);
    let (ticker, _ticker_task) = Ticker::spawn(HZ, 64).expect("valid tick rate");
    let (metrics_tx, metrics_rx) = mpsc::channel::<MetricsEvent>(512);
    let handle = tokio::spawn(
        Registry::new(
            rx,
            tx.clone(),
            factory,
            ticker,
            metrics_tx,
            None,
            None,
            Some(sink),
        )
        .run(),
    );
    (tx, metrics_rx, handle)
}

/// Wait for the room's drop event.
async fn dropped(metrics: &mut mpsc::Receiver<MetricsEvent>) -> MatchResultDrop {
    loop {
        let ev = tokio::time::timeout(WAIT, metrics.recv())
            .await
            .expect("the drop is reported in time")
            .expect("metrics open");
        if let MetricsEvent::MatchResultDropped(cause) = ev {
            return cause;
        }
    }
}

async fn stop(tx: Mailbox<RegistryMsg>, handle: tokio::task::JoinHandle<()>) {
    tx.send(RegistryMsg::Shutdown).await.expect("registry gone");
    drop(tx);
    handle.await.expect("registry task");
}

/// The one-slot sink already holds a result nobody read: the destroyed
/// room's result is refused as FULL, and counted.
#[tokio::test]
async fn a_result_a_full_sink_refused_is_counted() {
    let (sink, _unread) = channel::<MatchResult>(1);
    sink.try_send(MatchResult {
        room: RoomId(99),
        payload: bytes::Bytes::new(),
    })
    .expect("one slot");
    let (tx, mut metrics, handle) = start_observed(sink);
    create(&tx, RoomId(12)).await.expect("create failed");
    assert_eq!(destroy(&tx, RoomId(12)).await, RoomStatus::Destroyed);
    assert_eq!(dropped(&mut metrics).await, MatchResultDrop::Full);
    stop(tx, handle).await;
}

/// The consumer dropped its receiver: the result is refused as CLOSED,
/// and counted apart.
#[tokio::test]
async fn a_result_a_closed_sink_refused_is_counted_apart() {
    let (sink, gone) = channel::<MatchResult>(4);
    drop(gone);
    let (tx, mut metrics, handle) = start_observed(sink);
    create(&tx, RoomId(13)).await.expect("create failed");
    assert_eq!(destroy(&tx, RoomId(13)).await, RoomStatus::Destroyed);
    assert_eq!(dropped(&mut metrics).await, MatchResultDrop::Closed);
    stop(tx, handle).await;
}
