//! The export seam: every report the collector folds reaches every
//! exporter, in order — including the final report at
//! shutdown.

use super::*;

/// A tick feed at 200 Hz (the collector only wakes on ticks).
fn feed(tx: broadcast::Sender<TickInfo>) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut tick = 1u64;
        loop {
            tx.send(TickInfo {
                tick,
                at: Instant::now(),
            })
            .ok();
            tick += 1;
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
}

/// Two exporters and a channel sink: each report the sink gets reached
/// exporter A, then exporter B (the same report — its emission stamp
/// and its room), and the final report on a closed ticker reaches all
/// three too.
#[tokio::test]
async fn every_report_reaches_every_exporter_in_order() {
    let (tick_tx, _first) = broadcast::channel(64);
    let feeder = feed(tick_tx.clone());
    let (m_tx, m_rx) = mpsc::channel::<MetricsEvent>(64);
    let (sink_tx, mut sink_rx) = mpsc::unbounded_channel::<MetricReport>();
    // One ordered log of (who, emission stamp, room count).
    let (log_tx, mut log_rx) = mpsc::unbounded_channel::<(&'static str, Instant, usize)>();
    let exporter = |who: &'static str| {
        let log = log_tx.clone();
        Box::new(move |r: &MetricReport| {
            let _ = log.send((who, r.emitted_at, r.rooms.len()));
        }) as Box<dyn Exporter>
    };
    let collector = MetricsCollector::new(
        tick_tx.subscribe(),
        m_rx,
        MetricSink::Channel(sink_tx),
        Duration::from_millis(20),
    )
    .with_exporters(vec![exporter("a"), exporter("b")]);
    let task = tokio::spawn(collector.run());

    let t0 = Instant::now();
    m_tx.try_send(MetricsEvent::Room(room_sample(RoomId(3), t0, 10)))
        .expect("room sample queued");
    tokio::time::sleep(Duration::from_millis(80)).await;
    feeder.abort();
    drop(tick_tx);
    tokio::time::timeout(Duration::from_secs(3), task)
        .await
        .expect("collector exits on a closed ticker")
        .expect("collector task panicked");

    let mut reports = Vec::new();
    while let Ok(r) = sink_rx.try_recv() {
        reports.push(r);
    }
    assert!(reports.len() >= 3, "periodic reports + the final one");
    for r in &reports {
        let a = log_rx.try_recv().expect("exporter a saw the report");
        let b = log_rx.try_recv().expect("exporter b saw the report");
        assert_eq!(a, ("a", r.emitted_at, r.rooms.len()));
        assert_eq!(b, ("b", r.emitted_at, r.rooms.len()));
    }
    assert!(log_rx.try_recv().is_err(), "no report the sink did not get");
    assert_eq!(reports.last().map(|r| r.rooms.len()), Some(1));
}
