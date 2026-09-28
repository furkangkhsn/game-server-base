//! The registry's own sample on a full metrics channel (BACKLOG F2 — the
//! room's, the connection's and the transport's have their tests; the
//! final sample's is in `run::leftovers::tests`). A sample the full
//! channel dropped is counted in `metrics_dropped`, and loses nothing:
//! the registry's counters are cumulative, so the next sample carries
//! what the dropped one would have said. A CLOSED channel is not a drop
//! (the collector is gone; nothing reads another sample).

use std::sync::Arc;

use tokio::sync::mpsc;

use crate::channel::channel;
use crate::conn::ConnIn;
use crate::id::{ConnectionId, RoomId};
use crate::metrics::{MetricsEvent, RegistrySample};
use crate::registry::actor::Registry;
use crate::registry::{RegistryMsg, RoomFactory};
use crate::ticker::Ticker;

type Reg = Registry<(), (), (), ()>;

/// A registry (not running: the test calls its arms) on a metrics
/// channel of ONE slot, already taken; the channel's receiver.
fn registry() -> (Reg, mpsc::Receiver<MetricsEvent>) {
    let (ticker, _task) = Ticker::spawn(60.0, 64).expect("valid tick rate");
    let factory: RoomFactory<(), (), (), ()> =
        Arc::new(|_id, _cfg| unreachable!("no room is created here"));
    let (tx, rx) = channel::<RegistryMsg>(8);
    let (metrics, metrics_rx) = mpsc::channel(1);
    metrics
        .try_send(MetricsEvent::RoomGone(RoomId(0)))
        .expect("the one slot");
    let reg: Reg = Registry::new(rx, tx, factory, ticker, metrics, None, None, None);
    (reg, metrics_rx)
}

/// A connection opens: the registry flushes a sample.
async fn open(reg: &mut Reg, n: u64) {
    let (inbox, _conn) = channel::<ConnIn>(1);
    reg.on_conn_opened(ConnectionId(n), inbox).await;
}

/// The next event on the channel: the registry's sample.
fn sample(rx: &mut mpsc::Receiver<MetricsEvent>) -> RegistrySample {
    match rx.try_recv() {
        Ok(MetricsEvent::Registry(s)) => s,
        other => panic!("a registry sample, got {other:?}"),
    }
}

/// The first open's sample meets the full channel: dropped and counted.
/// The next sample carries both opens and the drop; the one after still
/// says one drop (cumulative, like every registry counter).
#[tokio::test]
async fn a_dropped_sample_is_counted_and_the_next_carries_its_counts() {
    let (mut reg, mut rx) = registry();
    open(&mut reg, 1).await;
    assert!(matches!(rx.try_recv(), Ok(MetricsEvent::RoomGone(_))));
    assert!(rx.try_recv().is_err(), "the first sample was dropped");
    open(&mut reg, 2).await;
    let s = sample(&mut rx);
    assert_eq!((s.opens, s.conns), (2, 2), "the dropped open is carried");
    assert_eq!(s.metrics_dropped, 1, "the drop, counted");
    open(&mut reg, 3).await;
    let s = sample(&mut rx);
    assert_eq!((s.opens, s.metrics_dropped), (3, 1));
}

/// The room-gone notice meets the full channel: counted with the
/// samples.
#[tokio::test]
async fn a_dropped_room_gone_notice_is_counted() {
    let (mut reg, _rx) = registry();
    reg.emit_room_gone(RoomId(5));
    assert_eq!(reg.sample().metrics_dropped, 1);
}

/// The collector is gone: a refused sample is not a dropped one.
#[tokio::test]
async fn a_closed_channel_is_not_a_drop() {
    let (mut reg, rx) = registry();
    drop(rx);
    open(&mut reg, 1).await;
    reg.emit_room_gone(RoomId(5));
    assert_eq!(reg.sample().metrics_dropped, 0);
    assert_eq!(reg.sample().opens, 1, "the open itself happened");
}
