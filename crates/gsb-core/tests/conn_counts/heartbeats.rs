//! The heartbeat throttle's surplus (SECURITY §3.2, BACKLOG B56): the
//! actor answers at most one heartbeat per second and counts the rest,
//! per phase — and until B56 those counts reached only a debug line.
//! Now they ride the samples, as deltas.

use std::ops::Range;

use super::*;
use rig::Conn;

/// Heartbeats numbered `ticks` (each answer echoes its own tick, so the
/// test knows which ones were answered).
async fn heartbeats(c: &Conn, ticks: Range<u64>) {
    for tick in ticks {
        c.send(op::base::HEARTBEAT, Heartbeat { tick }.encode_to_vec())
            .await;
    }
}

/// The ticks of every heartbeat ACK among `frames`.
fn acked(frames: &[FrameBody]) -> Vec<u64> {
    frames
        .iter()
        .filter(|f| f.op == op::base::HEARTBEAT_ACK)
        .map(|f| {
            base::HeartbeatAck::decode(f.payload.as_ref())
                .expect("HeartbeatAck decode")
                .tick
        })
        .collect()
}

/// Four heartbeats before authentication and four after (a burst of
/// three, then one more past the interval): the first of each burst is
/// answered (auth success resets the throttle), the rest are counted in
/// their own phase's counter. The pauses let the actor flush between the
/// bursts (its flush interval is 500 ms): each surplus is carried once,
/// however many samples the session sends.
///
/// The count is checked against what the actor really ANSWERED — every
/// heartbeat is either answered (an ACK echoing its tick) or counted,
/// exactly — not against "the first of each burst": a run stalled for
/// the throttle's whole interval inside a burst is answered twice there,
/// legitimately (BACKLOG F52). A burst with nothing left unanswered has
/// nothing to check and fails as an inconclusive run.
#[tokio::test]
async fn the_unanswered_heartbeats_reach_the_samples_by_phase() {
    let mut c = Conn::open(64);
    heartbeats(&c, 0..4).await;
    tokio::time::sleep(Duration::from_millis(600)).await;
    c.send_auth().await; // flushes the pre-auth burst
    let mut frames = c.frames_until(op::base::AUTH_RESULT).await;
    heartbeats(&c, 4..7).await;
    tokio::time::sleep(Duration::from_millis(1_100)).await;
    heartbeats(&c, 7..8).await; // flushes the authed burst
    let (sum, rest) = c.close_with_frames().await;
    frames.extend(rest);
    let acked = acked(&frames);
    let pre = acked.iter().filter(|&&t| t < 4).count() as u64;
    let authed = acked.len() as u64 - pre;
    assert!(acked.contains(&0) && acked.contains(&4), "{acked:?}");
    assert!(
        pre < 4 && authed < 4,
        "inconclusive run: a burst was answered whole ({acked:?}) — the \
         actor stalled for the 1 s interval inside it"
    );
    assert_eq!(sum.heartbeats_throttled_preauth, 4 - pre, "{acked:?}");
    assert_eq!(sum.heartbeats_throttled_authed, 4 - authed, "{acked:?}");
    assert_eq!(sum.violations, 0, "not a violation");
}
