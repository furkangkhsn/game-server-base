//! The handshake's re-send cap on the wire (BACKLOG B86): a step the
//! server never answers is re-sent on the band's doubling timer only up
//! to `HANDSHAKE_MAX_RTO` (200 ms) — 50, 100, 200, 200, … ms apart, not
//! 50, 100, 200, 400, 800. On a paused clock against a silent peer, so
//! the schedule is exact: every copy is in the peer's queue when the
//! handshake gives up. Child of `handshake`.

use super::*;

/// A peer that never answers: within 1 s the challenge request goes out
/// at 0, 50, 150, 350, 550, 750 and 950 ms — seven copies (the band's
/// uncapped doubling would send five: 0, 50, 150, 350, 750).
#[tokio::test(start_paused = true)]
async fn an_unanswered_step_is_re_sent_at_the_cap() {
    let silent = UdpSocket::bind("127.0.0.1:0").await.expect("bind");
    let addr = silent.local_addr().expect("addr");
    let gave_up = UdpClient::connect_within(addr, Duration::from_secs(1)).await;
    assert_eq!(
        gave_up.err().map(|e| e.kind()),
        Some(std::io::ErrorKind::TimedOut),
        "nobody answered"
    );
    let mut buf = [0u8; 64];
    let mut copies = 0;
    while let Ok((n, _)) = silent.try_recv_from(&mut buf) {
        assert_eq!(n, 18, "a challenge request");
        assert_eq!(buf[0], KIND_HELLO);
        assert_eq!(buf[9..17], [0u8; 8], "no cookie yet");
        copies += 1;
    }
    assert_eq!(copies, 7, "the first send and six re-sends, capped");
}
