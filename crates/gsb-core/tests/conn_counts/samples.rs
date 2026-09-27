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

/// The metrics channel's one slot is taken when the actor flushes on the
/// heartbeat: that sample is dropped. Drained, the final flush carries
/// every delta — the dropped sample's included — and the drop itself.
#[tokio::test]
async fn a_sample_dropped_on_a_full_channel_keeps_its_deltas() {
    let (mut c, spare) = Conn::open_with(64, 1);
    spare
        .try_send(MetricsEvent::RoomGone(RoomId(0)))
        .expect("the one slot was free");
    c.auth().await;
    tokio::time::sleep(PAST_FLUSH).await;
    c.send(op::base::HEARTBEAT, Heartbeat { tick: 1 }.encode_to_vec())
        .await;
    c.until(op::base::HEARTBEAT_ACK).await;
    // The flush on the heartbeat found the slot taken: nothing but the
    // filler is on the channel.
    assert!(matches!(c.take_metric(), Some(MetricsEvent::RoomGone(_))));
    assert!(c.take_metric().is_none(), "the flushed sample was dropped");
    let sum = c.close().await;
    assert_eq!(sum.frames_in, 2, "AUTH and the heartbeat");
    assert_eq!(sum.frames_out, 2, "the AUTH result and the ACK");
    assert!(sum.bytes_in > 0 && sum.bytes_out > 0);
    assert_eq!(sum.metrics_dropped, 1, "the dropped sample, counted");
    assert!(sum.last);
}
