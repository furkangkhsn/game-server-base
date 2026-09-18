//! The connection actor's own sample: `violations`, `frames_in`,
//! `frames_out`, and the `last` flag that closes the series.
//!
//! The violation BUDGET is what this binary's other tests drive, and they
//! drive it thoroughly — but through the ERROR frames the client
//! receives. The counter is the aggregate: `NetReport::violations` is
//! documented as "0 on a healthy server", and it is the only number an
//! operator can watch, since the per-event detail is a tracing event at
//! budget exhaustion that nothing sums. Neither it nor the frame deltas
//! nor `last` had ever been read back from a sample.
//!
//! `last` matters beyond bookkeeping: the collector uses it to retire a
//! closed connection's `actions_dropped` entry from the live
//! worst-offenders list into the cumulative total (see
//! `metrics::tests::pruning`). A final flush that forgot to set it would
//! leave gone connections named in the report forever.
//!
//! Synchronization: the actor flushes at most once per
//! `METRICS_FLUSH_EVERY`, but ALWAYS once more at close — so these tests
//! read the FINAL sample, taken after the actor task has exited. Nothing
//! is timing-dependent.

use super::*;

use gsb_core::metrics::ConnSample;

/// [`spawn_actor`] keeping the metrics receiver.
fn spawn_observed(
    conn: u64,
) -> (
    mpsc::Sender<ConnIn>,
    mpsc::Receiver<FrameBatch>,
    mpsc::Receiver<MetricsEvent>,
    JoinHandle<()>,
) {
    let (inbox_tx, inbox) = channel::<ConnIn>(128);
    let (out_tx, out_rx) = channel::<FrameBatch>(16);
    let (reg_tx, _reg_rx) = channel::<RegistryMsg>(16);
    let (metrics_tx, metrics_rx) = mpsc::channel::<MetricsEvent>(64);
    let actor = ConnectionActor::new(
        ConnectionId(conn),
        SocketAddr::from(([127, 0, 0, 1], 41_000u16 + conn as u16)),
        Arc::new(test_table()),
        reg_tx,
        inbox,
        out_tx,
        metrics_tx,
        None,
    );
    let h = tokio::spawn(actor.run());
    (inbox_tx, out_rx, metrics_rx, h)
}

/// Every `ConnSample` this actor emitted, oldest first. The fields are
/// DELTAS since the previous flush, so a test that wants totals sums
/// them — which is also what the collector does.
fn samples(metrics: &mut mpsc::Receiver<MetricsEvent>) -> Vec<ConnSample> {
    let mut v = Vec::new();
    while let Ok(ev) = metrics.try_recv() {
        if let MetricsEvent::Conn(s) = ev {
            v.push(s);
        }
    }
    v
}

/// Three unknown base-band opcodes (hard violations, weight 4 each —
/// under the budget of 16, so the connection survives them), then a
/// clean close.
///
/// The counters must show three violations, three frames in and three
/// frames out, and the series must END with `last = true`.
///
/// The frame counts are the separator that makes `violations` mean
/// something: they move on EVERY frame, in either direction, while
/// `violations` moves only on the ones the budget counted. A `violations`
/// wired to the inbound frame path would agree with this test's numbers
/// by coincidence — which is why the second test below sends frames that
/// are NOT violations and pins the two apart.
#[tokio::test]
async fn violations_frames_and_the_final_flag_reach_the_sample() {
    let (in_tx, mut out, mut metrics, handle) = spawn_observed(1);

    for i in 0..3u16 {
        in_tx
            .send(ConnIn::Frame(frame(42 + i, &[])))
            .await
            .expect("inbox open");
        let (code, _) = read_err(&mut out).await.expect("answered");
        assert_eq!(code, 1, "unknown opcode answers with code 1");
    }

    // Clean close: the inbox's sender is dropped, the actor's `recv`
    // returns `None` and it exits through its final flush.
    drop(in_tx);
    tokio::time::timeout(WAIT, handle)
        .await
        .expect("actor did not exit")
        .expect("actor task panicked");

    let all = samples(&mut metrics);
    assert!(
        !all.is_empty(),
        "the actor flushed at least the final sample"
    );
    assert!(
        all.last().expect("non-empty").last,
        "the FINAL sample must carry `last = true`: the collector retires \
         a closed connection's per-connection entry on that flag"
    );
    assert_eq!(
        all.iter().filter(|s| s.last).count(),
        1,
        "exactly one sample may claim to be the last"
    );

    let violations: u64 = all.iter().map(|s| s.violations).sum();
    let frames_in: u64 = all.iter().map(|s| s.frames_in).sum();
    let frames_out: u64 = all.iter().map(|s| s.frames_out).sum();
    assert_eq!(violations, 3, "three counted protocol violations");
    assert_eq!(frames_in, 3, "three frames were received");
    assert_eq!(
        frames_out, 3,
        "three ERROR frames were sent (the answered phase of the budget)"
    );
    assert!(
        all.iter().map(|s| s.bytes_in).sum::<u64>() > 0,
        "and their wire bytes were counted"
    );
}

/// Well-formed, ordinary frames move the FRAME counters and leave
/// `violations` alone — and the inbound and outbound counters are not
/// the same number.
///
/// This is the half that gives `NetReport::violations` its documented
/// meaning ("0 on a healthy server"): ordinary traffic must not
/// register. A `violations` accidentally wired to the inbound frame path
/// — the one mis-wiring the test above cannot see, because there every
/// inbound frame happened to be a violation — reads 2 here instead of 0,
/// and would have every healthy server looking like it was under attack.
///
/// Two heartbeats inside one throttle interval also split `frames_in`
/// from `frames_out` (2 in, 1 ack out): the §3.2 ACK throttle answers at
/// most one per second, and a surplus heartbeat is deliberately NOT a
/// violation — a chatty NAT keepalive is not hostile the way an
/// undefined opcode is.
#[tokio::test]
async fn ordinary_answered_frames_are_not_violations() {
    let (in_tx, mut out, mut metrics, handle) = spawn_observed(2);

    let hb = Heartbeat { tick: 1 }.encode_to_vec();
    for _ in 0..2 {
        in_tx
            .send(ConnIn::Frame(frame(op::base::HEARTBEAT, &hb)))
            .await
            .expect("inbox open");
    }

    // Clean close: the actor drains its inbox (both heartbeats) before
    // `recv` returns `None`, so no separate barrier is needed.
    drop(in_tx);
    tokio::time::timeout(WAIT, handle)
        .await
        .expect("actor did not exit")
        .expect("actor task panicked");

    let mut acks = 0;
    while let Ok(batch) = out.try_recv() {
        acks += batch
            .iter()
            .filter(|f| f.op == op::base::HEARTBEAT_ACK)
            .count();
    }
    assert_eq!(acks, 1, "the ACK throttle answers one of the two");

    let all = samples(&mut metrics);
    assert_eq!(
        all.iter().map(|s| s.violations).sum::<u64>(),
        0,
        "an answered heartbeat is ordinary traffic, and a throttled one is \
         explicitly not budgeted either: neither is a violation"
    );
    assert_eq!(
        all.iter().map(|s| s.frames_in).sum::<u64>(),
        2,
        "both heartbeats were counted inbound"
    );
    assert_eq!(
        all.iter().map(|s| s.frames_out).sum::<u64>(),
        1,
        "but only the one ACK the throttle let out was counted outbound"
    );
    assert!(all.last().expect("a final sample").last);
}
