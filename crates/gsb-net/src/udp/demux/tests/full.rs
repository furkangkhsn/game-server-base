//! Frames the demux drops on a session's full inbox, counted by kind and
//! sent to the collector (BACKLOG B58). Before, one per-session count fed
//! a warning and the stop log; an RPC request among them — already
//! acknowledged on the reliable band, so never re-sent — escaped the RPC
//! ledger.

use super::*;
use gsb_core::metrics::MetricsEvent;

/// The session's 16-deep inbox is filled with game frames; then a
/// request (reliable band), a game frame (raw band) and a heartbeat
/// (reliable band) meet it full. Dropping the demux sends its last
/// sample: one per kind.
#[tokio::test]
async fn frames_dropped_on_a_full_inbox_are_counted_by_kind() {
    let sock = Arc::new(
        UdpSocket::bind("127.0.0.1:0".parse::<SocketAddr>().unwrap())
            .await
            .expect("bind"),
    );
    let peer = "127.0.0.1:9".parse().unwrap();
    let (mut d, _in_rx) = demux_with_session(sock, peer);
    let (tx, mut rx) = tokio::sync::mpsc::channel(8);
    d.flusher = crate::metrics::Flusher::new(Some(tx));
    let game = || encode_raw(&FrameBody::new(1000, Bytes::from_static(b"move")));
    for _ in 0..16 {
        feed(&mut d, peer, &game());
    }
    feed(&mut d, peer, &rel_frame(1, op::base::RPC_REQ, b"req"));
    feed(&mut d, peer, &game());
    feed(&mut d, peer, &rel_frame(2, op::base::HEARTBEAT, b""));
    drop(d);
    let Ok(MetricsEvent::Transport(t)) = rx.try_recv() else {
        panic!("the demux's last sample");
    };
    assert_eq!(t.udp_requests_dropped_full, 1, "the request");
    assert_eq!(t.udp_actions_dropped_full, 1, "the game frame");
    assert_eq!(t.udp_control_frames_dropped_full, 1, "the heartbeat");
    assert_eq!(t.udp_datagrams_malformed, 0);
}
