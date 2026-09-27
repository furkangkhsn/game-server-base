//! End to end through the collector task: an actor's counters reach a
//! published report with the right rates, and the stale placeholder a
//! fresh watch starts on.

use super::*;

/// Receive reports until one carries room 1 in a state `ready` accepts;
/// returns that room line and its report. Every room line on the way
/// must keep the cumulative counters monotone against the previous one
/// (`seen`, updated as lines arrive).
///
/// WHY a wait and not a fixed window: the collector reports every period
/// of WALL time, on whichever ticks the real-time feed got in, so how
/// many steps one report window holds is the scheduler's call — on a
/// loaded machine the first report once held 2 (BACKLOG F23). The test
/// waits for the state it asserts; the bound below is only a hang guard.
async fn room_line_until(
    rx: &mut mpsc::UnboundedReceiver<MetricReport>,
    seen: &mut Option<RoomReport>,
    ready: impl Fn(&RoomReport) -> bool,
) -> (RoomReport, MetricReport) {
    let wait = async {
        loop {
            let report = rx.recv().await.expect("report channel open");
            let Some(r) = report.rooms.iter().find(|r| r.room == RoomId(1)).copied() else {
                continue;
            };
            if let Some(prev) = seen.replace(r) {
                assert!(r.steps >= prev.steps, "cumulative steps never decrease");
                assert!(
                    r.dropped >= prev.dropped,
                    "cumulative dropped never decreases"
                );
                assert!(
                    r.shipped_frames >= prev.shipped_frames,
                    "cumulative shipped frames never decrease"
                );
            }
            if ready(&r) {
                return (r, report);
            }
        }
    };
    match tokio::time::timeout(Duration::from_secs(10), wait).await {
        Ok(found) => found,
        Err(_) => panic!("no report reached the awaited room state; last line: {seen:?}"),
    }
}

/// The full path the spec asks for: counters live in the room actor's
/// local state, leave it over the (bounded) metrics channel via
/// `try_send`, and are accumulated by the collector task — asserted here
/// from the collector's reports, i.e. from outside the actor.
#[tokio::test]
async fn room_counters_flow_to_collector() {
    use crate::channel::{FrameBatch, Mailbox, channel};
    use crate::room::{Action, RoomActor, RoomConfig, RoomControl};
    use tokio::sync::oneshot;

    // One manual broadcast feeds both the room and the collector's
    // clock (the collector's only awaited source). A feed task keeps
    // ticks flowing in real time (100 Hz), exactly like the real
    // ticker — the collector only wakes on ticks, so the stream must
    // outlive the report window.
    let (tick_tx, _first) = broadcast::channel(64);
    let feed_task = {
        let tx = tick_tx.clone();
        tokio::spawn(async move {
            let mut tick = 1u64;
            loop {
                tx.send(TickInfo {
                    tick,
                    at: Instant::now(),
                })
                .ok();
                tick += 1;
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
    };
    let (m_tx, m_rx) = mpsc::channel::<MetricsEvent>(64);
    let (rep_tx, mut rep_rx) = mpsc::unbounded_channel::<MetricReport>();
    tokio::spawn(
        MetricsCollector::new(
            tick_tx.subscribe(),
            m_rx,
            MetricSink::Channel(rep_tx),
            Duration::from_millis(100),
        )
        .run(),
    );

    // The room: out channel capacity 2 and NO consumer, so after the
    // first batches the fan-out's try_send fails and `dropped` grows
    // — an actor-local counter that must reach the collector.
    // `metrics_cadence_hz = tick_hz` ⇒ sample every step, so this test's
    // ~100 ms report window is guaranteed to carry room samples (the
    // default 1 Hz cadence would first emit on step 30).
    let config = RoomConfig {
        id: RoomId(1),
        metrics_cadence_hz: 30.0,
        ..Default::default()
    };
    let (control, control_rx) = channel(config.control_capacity);
    let actor = RoomActor::new(
        config,
        (),
        Box::new(AlwaysLogic),
        tick_tx.subscribe(),
        control_rx,
        1,
        m_tx,
        None,
    );
    let room = tokio::spawn(actor.run());

    // Join one connection (its out channel is the room's fan-out target).
    let (out_tx, _out_rx) = mpsc::channel::<FrameBatch>(2);
    let (reply_tx, reply_rx) = oneshot::channel::<
        Result<(crate::id::EntityId, Mailbox<Action>), crate::error::CoreError>,
    >();
    control
        .send(RoomControl::Join {
            conn: ConnectionId(7),
            out: out_tx,
            reply: reply_tx,
        })
        .await
        .expect("control accepts");

    // The join reply proves the room is alive and processing.
    tokio::time::timeout(Duration::from_secs(3), reply_rx)
        .await
        .expect("join reply")
        .expect("join reply dropped")
        .expect("join accepted (room not full)");

    // A report that shows the join and the room's first steps past it
    // (drops start two batches after the join): the actor's counters,
    // read from outside the actor.
    let mut seen = None;
    let (r, _) = room_line_until(&mut rep_rx, &mut seen, |r| {
        r.members == 1 && r.steps >= 4 && r.dropped > 0
    })
    .await;
    assert_eq!(r.members, 1, "the join reached the room AND the report");
    assert_eq!(r.joins, 1);
    assert_eq!(r.groups, 1);
    assert_eq!(r.max_group, 1);
    assert!(
        r.steps >= 4,
        "room stepped on the fed ticks: {} steps",
        r.steps
    );
    assert!(r.snapshots > 0, "snapshots were encoded and counted");
    assert!(
        r.dropped > 0,
        "the unconsumed out channel must have produced drops, visible outside the actor"
    );
    assert!(
        r.shipped_frames > 0,
        "the fan-out's frame count reached the report over the real path \
         (actor counter -> sample -> accumulator -> report)"
    );
    assert!(r.step_max_us > 0, "step duration was measured");
    assert_eq!(r.step_hist.iter().sum::<u64>(), r.steps);

    // A later report: the room kept stepping and the counters kept
    // flowing (every line on the way was checked monotone).
    let (r2, second) = room_line_until(&mut rep_rx, &mut seen, |x| x.steps > r.steps).await;
    assert!(
        r2.steps > r.steps,
        "more steps in the second window: {} > {}",
        r2.steps,
        r.steps
    );
    assert!(
        r2.dropped >= r.dropped,
        "cumulative dropped never decreases"
    );
    // Net/registry scopes are absent here (no registry/conn actors in
    // this test): the render still has exactly one line (the room).
    assert_eq!(second.render().len(), 2); // room line + net line

    // Shutdown: abort the feed and drop the tick sender (closes the
    // broadcast); the room exits on Closed and the collector emits
    // one final report.
    feed_task.abort();
    drop(tick_tx);
    tokio::time::timeout(Duration::from_secs(3), room)
        .await
        .expect("room exited on closed ticker")
        .expect("room task panicked");
}
