//! `RegistrySample::rooms_died` — the "game logic panicked somewhere"
//! signal.
//!
//! The reap BEHAVIOUR is locked by this binary's other tests (the room
//! leaves the table, its members are notified, a restart-configured room
//! is rebuilt). The counter is a different claim and a load-bearing one:
//! its doc says non-zero means game logic panicked, and it is the only
//! aggregate an operator has for that — the per-room detail is a `warn`
//! at death time, which nothing aggregates. Two mis-wirings would be
//! invisible without this test: a death that does not count (the signal
//! is silently always 0), and a death counted as an ordinary destroy
//! (a panicking server looks like a busy control plane). The second half
//! is asserted from the other side too, in
//! `registry::counters::rooms` — an ordinary destroy must leave
//! `rooms_died` at 0.

use super::*;

use gsb_core::metrics::{MetricsEvent, RegistrySample};

/// [`start`] keeping the metrics receiver.
pub(super) fn start_observed(
    factory: RoomFactory<(), (), (), ()>,
) -> (
    Mailbox<RegistryMsg>,
    mpsc::Receiver<MetricsEvent>,
    tokio::task::JoinHandle<()>,
) {
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
            None,
        )
        .run(),
    );
    (tx, metrics_rx, handle)
}

/// Drain the metrics channel and return the registry's newest sample, if
/// any (the registry samples on state change, so a quiet interval emits
/// nothing).
fn latest_opt(metrics: &mut mpsc::Receiver<MetricsEvent>) -> Option<RegistrySample> {
    let mut last = None;
    while let Ok(ev) = metrics.try_recv() {
        if let MetricsEvent::Registry(s) = ev {
            last = Some(s);
        }
    }
    last
}

/// A room whose logic panics mid-tick is reaped, and the reap counts in
/// `rooms_died` — NOT in `rooms_destroyed`.
///
/// The `rooms_destroyed` half is what makes the signal usable: the two
/// sit on the same report line, and a death counted as a destroy would
/// make a server whose game logic is exploding on every room look
/// exactly like a healthy one with a busy control plane.
#[tokio::test]
async fn an_unexpected_death_counts_in_rooms_died_not_rooms_destroyed() {
    let (tx, mut metrics, handle) =
        start_observed(std::sync::Arc::new(|_id, _cfg| BuiltRoom::Single {
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
    // The join arms the bomb: the logic panics in the SYSTEMS phase of
    // the same step whose CONTROL phase admitted this member.
    let _ = spawn(&tx, ConnectionId(1), RoomId(1)).await;

    // The death report travels through two spawned tasks before the
    // table changes, so wait for the table first (the same barrier the
    // behaviour tests use), then read the sample.
    status_until(&tx, RoomId(1), |s| matches!(s, RoomStatus::Absent)).await;

    let deadline = tokio::time::Instant::now() + WAIT;
    let mut s = latest_opt(&mut metrics).expect("the registry sampled during setup");
    while s.rooms_died != 1 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the reaped room was never counted as a death; last sample = {s:?}"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
        if let Some(fresh) = latest_opt(&mut metrics) {
            s = fresh;
        }
    }
    assert_eq!(
        s.rooms_destroyed, 0,
        "an unexpected death is NOT a control-plane destroy: counting it \
         there would hide a panicking game logic behind ordinary churn"
    );
    assert_eq!(s.rooms_created, 1, "the room was created exactly once");
    assert_eq!(s.rooms, 0, "and it is gone from the table");

    tx.send(RegistryMsg::Shutdown).await.expect("registry gone");
    tokio::time::timeout(WAIT, handle)
        .await
        .expect("registry did not stop")
        .expect("registry task panicked");
}
