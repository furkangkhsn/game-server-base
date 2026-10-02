//! The demux's half of the game band's feedback: a client's REPORT goes
//! to its session's writer (the way an ACK does) and nowhere else; what
//! cannot go is counted. And the rule that made the new kind safe for an
//! older server: a kind the demux does not know is malformed — counted,
//! dropped, the session untouched.

use super::*;

/// A session whose writer channel (capacity `out_cap`) the test holds.
async fn with_writer(
    out_cap: usize,
) -> (
    Demux,
    SocketAddr,
    gsb_core::channel::Inbox<gsb_core::conn::ConnIn>,
    gsb_core::channel::Inbox<gsb_core::channel::FrameBatch>,
) {
    let sock = Arc::new(UdpSocket::bind("127.0.0.1:0").await.expect("bind"));
    let peer: SocketAddr = "127.0.0.1:9".parse().unwrap();
    let (mut d, in_rx) = demux_with_session(sock, peer);
    let (out_tx, out_rx) = gsb_core::channel::channel(out_cap);
    d.sessions.at_mut(&peer).unwrap().out_tx = out_tx;
    (d, peer, in_rx, out_rx)
}

/// A report reaches the writer as a `UDP_REPORT` frame carrying its two
/// fields; the connection actor never sees it, and it is no sign of life
/// for the idle window (an ACK is not either).
#[tokio::test]
async fn a_report_goes_to_the_sessions_writer_only() {
    let (mut d, peer, mut in_rx, mut out_rx) = with_writer(4).await;
    let seen = d.sessions[&peer].last_seen;
    feed(&mut d, peer, &encode_report(3, 7));
    let batch = out_rx.try_recv().expect("handed to the writer");
    assert_eq!(batch.len(), 1);
    assert_eq!(batch[0].op, gsb_protocol::op::base::UDP_REPORT);
    assert_eq!(parse_two_u32(&batch[0].payload), Some((3, 7)));
    assert!(in_rx.try_recv().is_err(), "the actor never sees it");
    assert_eq!(d.sessions[&peer].last_seen, seen, "not a sign of life");
    assert_eq!(
        (d.bad_datagrams, d.no_session, d.reports_not_forwarded),
        (0, 0, 0)
    );
}

/// A REPORT too short for its two fields is malformed; one from an
/// address with no session counts there; one the full writer channel
/// refuses is lost, counted.
#[tokio::test]
async fn a_report_that_cannot_go_is_counted() {
    let (mut d, peer, _in_rx, mut out_rx) = with_writer(1).await;
    feed(&mut d, peer, &encode_report(1, 2)[..8]);
    assert_eq!(d.bad_datagrams, 1, "a short report");
    feed(
        &mut d,
        "127.0.0.1:10".parse().unwrap(),
        &encode_report(1, 2),
    );
    assert_eq!(d.no_session, 1, "no session");
    feed(&mut d, peer, &encode_report(1, 2));
    feed(&mut d, peer, &encode_report(2, 2));
    assert_eq!(
        d.reports_not_forwarded, 1,
        "the second found the channel full"
    );
    assert!(out_rx.try_recv().is_ok());
    drop(out_rx);
    feed(&mut d, peer, &encode_report(3, 2));
    assert_eq!(d.reports_not_forwarded, 2, "a closed writer loses it too");
    assert!(!d.sessions.contains_key(&peer), "and its session goes");
}

/// A datagram of a kind this demux does not know (as a REPORT was to the
/// server before it existed) is malformed: counted, dropped, and the
/// session goes on as if it never came.
#[tokio::test]
async fn an_unknown_kind_is_malformed_and_leaves_the_session_alone() {
    let (mut d, peer, mut in_rx, mut out_rx) = with_writer(4).await;
    let seen = d.sessions[&peer].last_seen;
    feed(&mut d, peer, &[7, 1, 0, 0, 0, 2, 0, 0, 0]);
    assert_eq!(d.bad_datagrams, 1);
    assert!(d.sessions.contains_key(&peer));
    assert_eq!(d.sessions[&peer].last_seen, seen);
    assert!(in_rx.try_recv().is_err() && out_rx.try_recv().is_err());
    // The session still works.
    feed(
        &mut d,
        peer,
        &encode_raw(&FrameBody::new(1000, Bytes::from_static(b"x"))),
    );
    assert!(in_rx.try_recv().is_ok(), "its next frame is delivered");
}
