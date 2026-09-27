//! The refused match result's event is not lost to a full metrics
//! channel (BACKLOG B62): before, `send_match_result` told the collector
//! with a `try_send`, and a full channel dropped the event uncounted.

use super::*;
use crate::id::RoomId;

fn result() -> MatchResult {
    MatchResult {
        room: RoomId(5),
        payload: bytes::Bytes::new(),
    }
}

/// The sink refuses (closed) while the metrics channel's one slot is
/// taken: the event arrives once the collector reads.
#[tokio::test]
async fn a_refused_result_is_told_past_a_full_metrics_channel() {
    let (sink, gone) = crate::channel::channel::<MatchResult>(1);
    drop(gone);
    let (metrics, mut rx) = mpsc::channel(1);
    metrics
        .try_send(MetricsEvent::RoomGone(RoomId(0)))
        .expect("the one slot was free");
    assert_eq!(
        send_match_result(&sink, result(), &metrics),
        Err(MatchResultDrop::Closed)
    );
    let wait = std::time::Duration::from_secs(5);
    assert!(matches!(
        tokio::time::timeout(wait, rx.recv()).await,
        Ok(Some(MetricsEvent::RoomGone(_)))
    ));
    assert!(matches!(
        tokio::time::timeout(wait, rx.recv()).await,
        Ok(Some(MetricsEvent::MatchResultDropped(
            MatchResultDrop::Closed
        )))
    ));
}
