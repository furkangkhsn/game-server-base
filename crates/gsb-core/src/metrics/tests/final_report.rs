//! The collector's final report waits for the producers' last words
//! (BACKLOG F35): a room's `RoomFinal` and a connection's final flush
//! sent AFTER the ticker closed — the server's stop signal, on which the
//! rooms and connections only start their ends — are in it; a producer
//! that outlives the grace does not hold it; the transport channel's
//! close is not waited for.
//!
//! Paused clock: the grace is a tokio deadline, so "exactly at the grace"
//! and "without waiting at all" are exact.

use super::*;

/// A connection's final flush with `frames_in` frames counted.
fn conn_final(conn: u64, frames_in: u64) -> ConnSample {
    ConnSample {
        conn: ConnectionId(conn),
        bytes_in: 0,
        bytes_out: 0,
        frames_in,
        frames_out: 0,
        actions_dropped: 0,
        metrics_dropped: 0,
        violations: 0,
        input_rate_limited: 0,
        actions_dropped_closed: 0,
        requests_dropped_closed: 0,
        requests_dropped_full: 0,
        requests_no_room: 0,
        heartbeats_throttled_preauth: 0,
        heartbeats_throttled_authed: 0,
        frames_out_closed: 0,
        close_notices_dropped: 0,
        requests_unprocessed: 0,
        actions_unprocessed: 0,
        control_frames_unprocessed: 0,
        server_close: None,
        last: true,
        tickets: Default::default(),
    }
}

/// The collector over a channel sink; the test holds the ticker's sender
/// and the producers' sender.
struct Rig {
    ticks: broadcast::Sender<TickInfo>,
    events: mpsc::Sender<MetricsEvent>,
    reports: mpsc::UnboundedReceiver<MetricReport>,
    collector: tokio::task::JoinHandle<bool>,
}

fn rig(transport: Option<mpsc::Receiver<MetricsEvent>>) -> Rig {
    let (ticks, tick_rx) = broadcast::channel::<TickInfo>(4);
    let (events, rx) = mpsc::channel::<MetricsEvent>(16);
    let (sink, reports) = mpsc::unbounded_channel::<MetricReport>();
    let mut c = MetricsCollector::new(
        tick_rx,
        rx,
        MetricSink::Channel(sink),
        Duration::from_secs(1),
    );
    if let Some(t) = transport {
        c = c.with_transport_events(t);
    }
    Rig {
        ticks,
        events,
        reports,
        collector: tokio::spawn(c.run()),
    }
}

/// Every report the collector emitted, in order.
fn all(reports: &mut mpsc::UnboundedReceiver<MetricReport>) -> Vec<MetricReport> {
    std::iter::from_fn(|| reports.try_recv().ok()).collect()
}

/// The stop's order: the ticker closes first, the room (which never
/// reached its first periodic sample) and a connection finish their ends
/// a while later, then drop their senders. The one report is the final
/// one, and it carries both last words.
#[tokio::test(start_paused = true)]
async fn last_words_sent_after_the_ticker_closed_are_in_the_final_report() {
    let Rig {
        ticks,
        events,
        mut reports,
        collector,
    } = rig(None);
    drop(ticks);
    // The room's teardown hooks and the connection's Shutdown take time.
    tokio::time::sleep(Duration::from_millis(300)).await;
    let mut last = room_sample(RoomId(5), Instant::now(), 12);
    last.requests_dropped_unread = 3;
    let _ = events.try_send(MetricsEvent::RoomFinal(last));
    let _ = events.try_send(MetricsEvent::Conn(conn_final(9, 7)));
    drop(events);

    let complete = collector.await.expect("collector task");
    assert!(complete, "every producer dropped its sender");
    let reports = all(&mut reports);
    let fin = reports.last().expect("a final report");
    assert_eq!(fin.rooms.len(), 1, "the room is in the final report");
    assert_eq!(fin.rooms[0].room, RoomId(5));
    assert_eq!(fin.rooms[0].steps, 12, "its final count");
    assert_eq!(fin.rooms[0].requests_dropped_unread, 3);
    assert_eq!(fin.net.frames_in, 7, "the connection's final flush too");
}

/// A producer that never drops its sender does not hold the final report
/// past the grace: it goes out exactly then, with what did arrive, and
/// the collector says it was not complete.
#[tokio::test(start_paused = true)]
async fn a_producer_outliving_the_grace_does_not_hold_the_final_report() {
    let Rig {
        ticks,
        events,
        mut reports,
        collector,
    } = rig(None);
    let started = tokio::time::Instant::now();
    drop(ticks);
    events
        .try_send(MetricsEvent::RoomFinal(room_sample(
            RoomId(2),
            Instant::now(),
            4,
        )))
        .expect("queued");

    let complete = collector.await.expect("collector task");
    assert!(!complete, "a producer was still running at the grace");
    assert_eq!(started.elapsed(), FINAL_REPORT_GRACE, "exactly the grace");
    let reports = all(&mut reports);
    assert_eq!(reports.len(), 1, "one report: the final one");
    assert_eq!(reports[0].rooms.len(), 1, "what arrived is in it");
    drop(events);
}

/// The transport tasks' own channel is folded but not waited for: with
/// its sender still held (a transport task whose silent peer keeps its
/// socket open), the final report goes out as soon as the main channel's
/// producers are gone — and carries what the transport sent.
#[tokio::test(start_paused = true)]
async fn the_final_report_does_not_wait_for_the_transport_channel() {
    let (transport, transport_rx) = mpsc::channel::<MetricsEvent>(4);
    let Rig {
        ticks,
        events,
        mut reports,
        collector,
    } = rig(Some(transport_rx));
    let started = tokio::time::Instant::now();
    drop(ticks);
    let lost = TransportCounters {
        udp_acks_not_forwarded: 5,
        ..TransportCounters::default()
    };
    transport
        .try_send(MetricsEvent::Transport(lost))
        .expect("queued");
    drop(events);

    let complete = collector.await.expect("collector task");
    assert!(complete, "the main channel's producers are all gone");
    assert_eq!(
        started.elapsed(),
        Duration::ZERO,
        "no wait for the transport"
    );
    let reports = all(&mut reports);
    let fin = reports.last().expect("a final report");
    assert_eq!(fin.transport.udp_acks_not_forwarded, 5);
    drop(transport);
}

/// A transport's last word that lands while the collector is already
/// folding the stop — after the session producers' last event, before
/// their channel closes — is still in the final report: the transport
/// channel is drained once more when the main channel closes.
#[tokio::test(start_paused = true)]
async fn a_transport_word_after_the_last_session_event_is_in_the_final_report() {
    let (transport, transport_rx) = mpsc::channel::<MetricsEvent>(4);
    let Rig {
        ticks,
        events,
        mut reports,
        collector,
    } = rig(Some(transport_rx));
    drop(ticks);
    // The collector is folding the stop's last words by now.
    tokio::time::sleep(Duration::from_millis(10)).await;
    let _ = events.try_send(MetricsEvent::RoomFinal(room_sample(
        RoomId(3),
        Instant::now(),
        6,
    )));
    tokio::time::sleep(Duration::from_millis(10)).await;
    // A pump's final flush, after the room's word was folded.
    let lost = TransportCounters {
        udp_acks_not_forwarded: 4,
        ..TransportCounters::default()
    };
    transport
        .try_send(MetricsEvent::Transport(lost))
        .expect("queued");
    drop(events);

    assert!(collector.await.expect("collector task"));
    let reports = all(&mut reports);
    let fin = reports.last().expect("a final report");
    assert_eq!(fin.rooms.len(), 1, "the room's final count");
    assert_eq!(
        fin.transport.udp_acks_not_forwarded, 4,
        "the transport word that came after it"
    );
    drop(transport);
}
