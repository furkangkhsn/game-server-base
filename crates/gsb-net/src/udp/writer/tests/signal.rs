//! The writer tells its connection actor the session's path (BACKLOG
//! B103): each piece of news once, the newest when the inbox was full, a
//! fresh open state after a new IP — and, with the response off,
//! nothing at all.

use super::*;
use gsb_core::channel::Inbox;
use gsb_core::path::PathState as CorePath;

/// Every path state in the inbox, in order (anything else fails).
fn told(inbox: &mut Inbox<ConnIn>) -> Vec<CorePath> {
    let mut v = Vec::new();
    while let Ok(m) = inbox.try_recv() {
        match m {
            ConnIn::Path(p) => v.push(p),
            other => panic!("only path news was expected: {other:?}"),
        }
    }
    v
}

fn phases(v: &[CorePath]) -> Vec<PathPhase> {
    v.iter().map(|p| p.phase).collect()
}

/// The first report on this path measured it (open: news to a room that
/// knew nothing); a lossy interval suspects it, a second paces it; a ring
/// of unanswered probes halves the rate (news: more than a tenth). A
/// clean report that changes only the measurements is not news.
#[tokio::test]
async fn a_paced_writer_tells_its_actor_each_news_once() {
    let (mut w, _client, mut inbox) = writer_with_inbox(UdpCongestion::Pace, 8).await;
    w.apply_report(&report(0, 0));
    w.probe_pass();
    w.apply_report(&report(1, 0));
    let first = told(&mut inbox);
    assert_eq!(phases(&first), vec![PathPhase::Open]);
    assert!(
        first[0].rtt.is_some() && first[0].rate.is_none(),
        "{first:?}"
    );
    interval(&mut w, 2, 20); // clean
    assert_eq!(told(&mut inbox), vec![], "measurements alone are not news");
    interval(&mut w, 3, 25); // 15 of 20 lost
    interval(&mut w, 4, 30);
    let news = told(&mut inbox);
    assert_eq!(phases(&news), vec![PathPhase::Suspect, PathPhase::Paced]);
    let rate = news[1].rate.expect("paced: a rate");
    assert_eq!(Some(rate), w.path_state().rate);
    w.feedback.set_interval(Duration::ZERO);
    for _ in 0..=crate::udp::feedback::PROBE_RING {
        w.probe_pass();
    }
    let halved = told(&mut inbox);
    assert_eq!(halved.len(), 1);
    assert!(halved[0].rate.expect("paced") <= rate / 2 + 1, "{halved:?}");
}

/// A full inbox keeps the newest state owed: the suspect state that could
/// not go is never sent; the paced one goes with the next decision.
#[tokio::test]
async fn a_full_inbox_keeps_the_newest_state_owed() {
    // Two slots: the writer reserves one at birth for its death verdict
    // (B66), the other is the room the news has.
    let (mut w, _client, mut inbox) = writer_with_inbox(UdpCongestion::Pace, 2).await;
    w.apply_report(&report(0, 0));
    w.probe_pass();
    w.apply_report(&report(1, 0));
    assert_eq!(phases(&told(&mut inbox)), vec![PathPhase::Open]);
    w.in_tx
        .try_send(ConnIn::LeftRoom {
            room: gsb_core::id::RoomId(1),
        })
        .expect("room for the filler");
    interval(&mut w, 2, 5); // suspect: owed
    interval(&mut w, 3, 10); // paced: owed, replaces it
    assert!(matches!(inbox.try_recv(), Ok(ConnIn::LeftRoom { .. })));
    assert_eq!(told(&mut inbox), vec![], "nothing more until a decision");
    // The next decision brings no news of its own (the same state): the
    // owed paced state goes now — never the suspect one it replaced.
    let paced = w.path_state();
    w.pace_tell();
    assert_eq!(told(&mut inbox), vec![paced]);
    w.pace_tell();
    assert_eq!(told(&mut inbox), vec![], "delivered: owed no more");
}

/// A new IP starts the path over: the fresh open state is told even
/// though an open path carries no budget either way; a new port alone
/// keeps the path and tells nothing.
#[tokio::test]
async fn a_new_ip_tells_a_fresh_open_state() {
    let (mut w, _client, mut inbox) = writer_with_inbox(UdpCongestion::Pace, 8).await;
    w.apply_report(&report(0, 0));
    w.probe_pass();
    w.apply_report(&report(1, 0));
    assert_eq!(phases(&told(&mut inbox)), vec![PathPhase::Open]);
    let port = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    w.apply_path(&FrameBody::new(
        op::base::UDP_PATH,
        Bytes::from(crate::udp::path::encode_addr(port.local_addr().unwrap())),
    ));
    assert_eq!(told(&mut inbox), vec![], "the same path");
    let ip = UdpSocket::bind("127.0.0.2:0")
        .await
        .expect("loopback alias");
    w.apply_path(&FrameBody::new(
        op::base::UDP_PATH,
        Bytes::from(crate::udp::path::encode_addr(ip.local_addr().unwrap())),
    ));
    assert_eq!(
        told(&mut inbox),
        vec![CorePath::default()],
        "fresh and open"
    );
}

/// With the response off the writer posts nothing to its actor, whatever
/// the reports say and wherever the session moves: the inbox carries
/// what it always did.
#[tokio::test]
async fn with_the_response_off_the_actor_hears_nothing() {
    let (mut w, _client, mut inbox) = writer_with_inbox(UdpCongestion::Off, 8).await;
    w.apply_report(&report(0, 0));
    w.probe_pass();
    w.apply_report(&report(1, 0));
    interval(&mut w, 2, 5);
    interval(&mut w, 3, 10);
    w.feedback.set_interval(Duration::ZERO);
    for _ in 0..=crate::udp::feedback::PROBE_RING {
        w.probe_pass();
    }
    let ip = UdpSocket::bind("127.0.0.2:0")
        .await
        .expect("loopback alias");
    w.apply_path(&FrameBody::new(
        op::base::UDP_PATH,
        Bytes::from(crate::udp::path::encode_addr(ip.local_addr().unwrap())),
    ));
    assert!(inbox.try_recv().is_err(), "nothing told");
}
