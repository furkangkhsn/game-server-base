//! The flusher's rules (BACKLOG B58, with B59's): deltas since the last
//! sample the channel TOOK, a dropped sample counted and its deltas
//! kept, the last sample past a full channel, nothing sent for nothing.

use super::*;
use gsb_core::id::RoomId;

fn refused(n: u64) -> TransportCounters {
    TransportCounters {
        handshakes_refused: n,
        ..Default::default()
    }
}

fn take(rx: &mut mpsc::Receiver<MetricsEvent>) -> Option<TransportCounters> {
    match rx.try_recv() {
        Ok(MetricsEvent::Transport(t)) => Some(t),
        _ => None,
    }
}

#[tokio::test]
async fn a_dropped_sample_keeps_its_deltas_and_is_counted() {
    let (tx, mut rx) = mpsc::channel(1);
    let mut f = Flusher::new(Some(tx.clone()));
    f.flush(refused(2), false);
    assert_eq!(take(&mut rx), Some(refused(2)), "the first delta");
    tx.try_send(MetricsEvent::RoomGone(RoomId(0)))
        .expect("slot free");
    f.flush(refused(5), false);
    assert!(matches!(rx.try_recv(), Ok(MetricsEvent::RoomGone(_))));
    assert!(rx.try_recv().is_err(), "the second sample was dropped");
    f.flush(refused(6), false);
    let t = take(&mut rx).expect("the third sample");
    assert_eq!(t.handshakes_refused, 4, "5 - 2 kept, plus 1");
    assert_eq!(t.metrics_dropped, 1, "the drop, counted");
    f.flush(refused(6), false);
    assert!(rx.try_recv().is_err(), "nothing new, nothing sent");
}

#[tokio::test]
async fn the_last_sample_goes_out_past_a_full_channel() {
    let (tx, mut rx) = mpsc::channel(1);
    let mut f = Flusher::new(Some(tx.clone()));
    tx.try_send(MetricsEvent::RoomGone(RoomId(0)))
        .expect("slot free");
    f.flush(refused(3), true);
    assert!(matches!(rx.recv().await, Some(MetricsEvent::RoomGone(_))));
    let got = tokio::time::timeout(Duration::from_secs(5), rx.recv())
        .await
        .expect("delivered once the slot freed");
    assert!(matches!(got, Some(MetricsEvent::Transport(t)) if t == refused(3)));
}
