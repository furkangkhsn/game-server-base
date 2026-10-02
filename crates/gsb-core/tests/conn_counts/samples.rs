//! The counting itself (BACKLOG B59). A connection actor's samples are
//! DELTAS since its last flush, sent with a `try_send` on the bounded
//! metrics channel. Before B59 the actor advanced its flushed baseline
//! before the send, so a sample the full channel dropped took its deltas
//! with it: only `metrics_dropped` said something was lost, never what.

use super::*;
use gsb_core::id::RoomId;
use rig::Conn;

/// Longer than the actor's flush interval (500 ms): the next inbound
/// frame flushes.
const PAST_FLUSH: Duration = Duration::from_millis(600);

/// Authenticate, then take whatever the AUTH flushed. The interval runs
/// from the actor's birth, so an actor that reached its first frame more
/// than 500 ms after it was built (a starved run) flushes ON the AUTH —
/// before its result goes out, so the sample is on the channel by now.
/// Taken here, it leaves the channel's one slot to the test, and its
/// deltas still count (BACKLOG F52: the filler used to meet that sample
/// and count a second drop, or find the slot taken).
async fn auth_and_take_flushed(c: &mut Conn) -> Option<ConnSample> {
    c.auth().await;
    let mut early = c.take_samples();
    assert!(early.len() <= 1, "one slot: {early:?}");
    early.pop()
}

/// `sum` with the AUTH's early sample (if any) folded in front of it.
fn with_early(early: Option<ConnSample>, sum: ConnSample) -> ConnSample {
    match early {
        Some(e) => rig::add(e, sum),
        None => sum,
    }
}

/// The metrics channel's one slot is taken when the actor flushes on the
/// heartbeat: that sample is dropped. Drained, the final flush carries
/// every delta — the dropped sample's included — and the drop itself.
#[tokio::test]
async fn a_sample_dropped_on_a_full_channel_keeps_its_deltas() {
    let (mut c, spare) = Conn::open_with(64, 1);
    let early = auth_and_take_flushed(&mut c).await;
    spare
        .try_send(MetricsEvent::RoomGone(RoomId(0)))
        .expect("the one slot was free");
    // Past the interval from the last flush (the actor's birth or the
    // AUTH, both before the AUTH result): the heartbeat flushes.
    tokio::time::sleep(PAST_FLUSH).await;
    c.send(op::base::HEARTBEAT, Heartbeat { tick: 1 }.encode_to_vec())
        .await;
    c.until(op::base::HEARTBEAT_ACK).await;
    // The flush on the heartbeat found the slot taken: nothing but the
    // filler is on the channel.
    assert!(matches!(c.take_metric(), Some(MetricsEvent::RoomGone(_))));
    assert!(c.take_metric().is_none(), "the flushed sample was dropped");
    let sum = with_early(early, c.close().await);
    assert_eq!(sum.frames_in, 2, "AUTH and the heartbeat");
    assert_eq!(sum.frames_out, 2, "the AUTH result and the ACK");
    assert!(sum.bytes_in > 0 && sum.bytes_out > 0);
    assert_eq!(sum.metrics_dropped, 1, "the dropped sample, counted");
    assert!(sum.last);
}

/// The FINAL sample meets a full channel: it is not dropped — it goes
/// out from a spawned sender once the collector reads, with the verdict
/// it carries (before, the final sample, and every delta it held, were
/// lost, counted nowhere: the actor was gone).
#[tokio::test]
async fn the_final_sample_is_not_lost_to_a_full_channel() {
    let (mut c, spare) = Conn::open_with(64, 1);
    let early = auth_and_take_flushed(&mut c).await;
    spare
        .try_send(MetricsEvent::RoomGone(RoomId(0)))
        .expect("the one slot was free");
    c.tell(ConnIn::ServerClosed {
        cause: gsb_core::conn::ServerClose::IdleTimeout,
        reason: "idle".into(),
    })
    .await;
    // The actor has ended with the slot still taken: its final flush
    // met a full channel.
    c.actor_done().await;
    let mut events = Vec::new();
    tokio::time::timeout(WAIT, async {
        while events.len() < 2 {
            match c.take_metric() {
                Some(ev) => events.push(ev),
                None => tokio::time::sleep(Duration::from_millis(5)).await,
            }
        }
    })
    .await
    .expect("the filler and the final sample in time");
    let Some(MetricsEvent::Conn(last)) = events.pop() else {
        panic!("the final sample after the filler");
    };
    assert!(last.last);
    let early_in = early.map_or(0, |e| e.frames_in);
    assert_eq!(early_in + last.frames_in, 1, "the AUTH");
    assert_eq!(
        last.server_close,
        Some(gsb_core::conn::ServerClose::IdleTimeout)
    );
    assert_eq!(last.metrics_dropped, 0, "nothing was dropped");
}
