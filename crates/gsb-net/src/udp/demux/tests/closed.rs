//! What the demux loses past a session's end, counted (BACKLOG B66):
//! frames decoded for a session whose actor had closed its inbox (by
//! kind, including the in-order frames behind the refused one),
//! datagrams from an address with no session, and ACKs the socket
//! refused.

use super::*;
use gsb_core::metrics::{MetricsEvent, TransportCounters};

async fn bound() -> Arc<UdpSocket> {
    Arc::new(
        UdpSocket::bind("127.0.0.1:0".parse::<SocketAddr>().unwrap())
            .await
            .expect("bind"),
    )
}

fn last_sample(rx: &mut tokio::sync::mpsc::Receiver<MetricsEvent>) -> TransportCounters {
    let mut total = TransportCounters::default();
    while let Ok(ev) = rx.try_recv() {
        if let MetricsEvent::Transport(t) = ev {
            total.add(&t);
        }
    }
    total
}

/// The actor closed its inbox: an out-of-order request waits behind the
/// gap, the gap-filling heartbeat is refused, and both are lost with the
/// session — each counted by its kind. What the peer sends afterwards
/// finds no session.
#[tokio::test]
async fn frames_for_a_closed_session_are_counted_by_kind() {
    let peer = "127.0.0.1:9".parse().unwrap();
    let (mut d, mut in_rx) = demux_with_session(bound().await, peer);
    let (tx, mut rx) = tokio::sync::mpsc::channel(8);
    d.flusher = crate::metrics::Flusher::new(Some(tx));
    in_rx.close();
    // seq 2 first: buffered behind the gap.
    feed(&mut d, peer, &rel_frame(2, op::base::RPC_REQ, b"req"));
    // seq 1 fills the gap: refused, and seq 2 behind it goes too.
    feed(&mut d, peer, &rel_frame(1, op::base::HEARTBEAT, b""));
    assert!(d.sessions.is_empty(), "the session is removed at once");
    // After the removal: a game frame and an ACK from the same address.
    feed(
        &mut d,
        peer,
        &encode_raw(&FrameBody::new(1000, Bytes::from_static(b"move"))),
    );
    feed(&mut d, peer, &encode_ack(3));
    feed(&mut d, peer, &rel_frame(3, op::base::HEARTBEAT, b""));
    drop(d);
    let t = last_sample(&mut rx);
    assert_eq!(t.udp_requests_dropped_closed, 1, "the buffered request");
    assert_eq!(t.udp_control_frames_dropped_closed, 1, "the heartbeat");
    assert_eq!(t.udp_actions_dropped_closed, 0);
    assert_eq!(t.udp_datagrams_no_session, 3, "RAW, ACK and REL after it");
}

/// A RAW frame refused by a closed inbox is counted as a game frame.
#[tokio::test]
async fn a_raw_frame_for_a_closed_session_is_an_action() {
    let peer = "127.0.0.1:9".parse().unwrap();
    let (mut d, mut in_rx) = demux_with_session(bound().await, peer);
    let (tx, mut rx) = tokio::sync::mpsc::channel(8);
    d.flusher = crate::metrics::Flusher::new(Some(tx));
    in_rx.close();
    feed(
        &mut d,
        peer,
        &encode_raw(&FrameBody::new(1000, Bytes::from_static(b"move"))),
    );
    drop(d);
    let t = last_sample(&mut rx);
    assert_eq!(t.udp_actions_dropped_closed, 1);
    assert_eq!(t.udp_datagrams_no_session, 0);
}

/// An ACK the socket refuses (an IPv6 peer on an IPv4 socket: the send
/// fails at once, every time) is counted.
#[tokio::test]
async fn an_ack_the_socket_refuses_is_counted() {
    let peer = "[::1]:9".parse().unwrap();
    let (mut d, _in_rx) = demux_with_session(bound().await, peer);
    let (tx, mut rx) = tokio::sync::mpsc::channel(8);
    d.flusher = crate::metrics::Flusher::new(Some(tx));
    feed(&mut d, peer, &rel_frame(1, op::base::HEARTBEAT, b""));
    drop(d);
    let t = last_sample(&mut rx);
    assert_eq!(t.udp_acks_send_failed, 1, "{t:?}");
}

/// A handshake challenge the socket refuses is counted.
#[tokio::test]
async fn a_challenge_the_socket_refuses_is_counted() {
    let peer = "[::1]:9".parse().unwrap();
    let (mut d, _end_rx) = demux_bare(bound().await);
    let (tx, mut rx) = tokio::sync::mpsc::channel(8);
    d.flusher = crate::metrics::Flusher::new(Some(tx));
    // A challenge request: a HELLO carrying no cookie.
    feed(&mut d, peer, &encode_hello(7, 0));
    assert_eq!(d.challenges, 1, "the request was answered");
    drop(d);
    let t = last_sample(&mut rx);
    assert_eq!(t.udp_challenges_send_failed, 1, "{t:?}");
}
