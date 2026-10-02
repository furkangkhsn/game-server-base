//! The end of a session (BACKLOG B128): whatever ended it — the reliable
//! band's liveness bound, a stateless reset, a record-layer limit — the
//! frames the client had already received are handed out first, and then
//! `recv_frame` reports the end AT ONCE (no window waited out, nothing
//! more read from the socket): the half `gsb_client::Conn::recv` turns
//! into `Recv::Closed`, as a stream's EOF.

use super::reset::sealed;
use super::seal::seal;
use super::*;
use crate::seal::{INTEGRITY_LIMIT, ResetToken, SEAL_LIMIT};

/// Longer than any test could wait by accident: a `recv_frame` on an
/// ended session that waits this long has missed the end.
const WINDOW: Duration = Duration::from_secs(2);

/// Read until the client reports nothing; the ops it handed out, and how
/// long the final (empty) read took.
async fn drain(c: &mut UdpClient) -> (Vec<u16>, Duration) {
    let mut ops = Vec::new();
    loop {
        let t0 = Instant::now();
        match c.recv_frame(WINDOW).await.expect("recv") {
            Some(f) => ops.push(f.op),
            None => return (ops, t0.elapsed()),
        }
    }
}

/// The drained frames, then the end at once — for its reason; and
/// nothing more goes out, or is counted, after it.
async fn assert_drained_then_ended(c: &mut UdpClient, why: UdpEnd, want: &[u16]) {
    assert!(!c.is_established());
    assert_eq!(c.ended(), Some(why));
    let (ops, last) = drain(c).await;
    assert_eq!(ops, want, "what was received before the end is handed out");
    assert!(
        last < WINDOW / 4,
        "the end is reported at once, not after {last:?}"
    );
    let (again, _) = drain(c).await;
    assert!(again.is_empty(), "and stays the end");
    let before = format!("{:?}", c.stats);
    for op in [gsb_protocol::op::base::HEARTBEAT, 1000] {
        let e = c
            .send_frame(op, Bytes::from_static(b"x"))
            .await
            .unwrap_err();
        assert_eq!(e.kind(), std::io::ErrorKind::NotConnected, "{e}");
    }
    assert_eq!(
        format!("{:?}", c.stats),
        before,
        "a refused send counts nothing"
    );
    assert_eq!(c.ended(), Some(why), "the first reason stands");
}

#[tokio::test]
async fn a_rel_dead_session_drains_then_ends_at_once() {
    let (mut c, _sink) = detached().await;
    // Two ordered frames received (not read yet), one beyond a gap.
    c.process_datagram(&rel(1, 20, b"a"));
    c.process_datagram(&rel(2, 21, b"b"));
    c.process_datagram(&rel(4, 23, b"d"));
    c.send_frame(gsb_protocol::op::base::HEARTBEAT, Bytes::from_static(b"hb"))
        .await
        .expect("send");
    c.rel.rewind_progress(REL_NO_ACK_FATAL);
    c.retransmit_pass();
    assert_drained_then_ended(&mut c, UdpEnd::RelDead, &[20, 21]).await;
    assert_eq!(c.stats.gave_up, 1, "the heartbeat, never delivered");
    assert_eq!(c.stats.oob_at_end, 1, "seq 4, stranded behind the gap");
}

#[tokio::test]
async fn a_stateless_reset_drains_then_ends_at_once() {
    let token = ResetToken::from_bytes([0x5A; 16]);
    let (mut c, _sink, mut ss) = sealed(token).await;
    c.process_datagram(&seal(&mut ss, &rel(1, 20, b"a")));
    let reset = crate::seal::reset_datagram(&token, 40, &[0xA7; crate::seal::RESET_LEN_MAX])
        .expect("a reset");
    c.process_datagram(&reset);
    assert_drained_then_ended(&mut c, UdpEnd::Reset, &[20]).await;
    assert_eq!(c.stats.stateless_resets_received, 1);
}

#[tokio::test]
async fn an_exhausted_record_counter_drains_then_ends_at_once() {
    let (mut c, _sink, mut ss) = sealed(ResetToken::from_bytes([0; 16])).await;
    c.process_datagram(&seal(&mut ss, &rel(1, 20, b"a")));
    c.seal
        .send_half()
        .expect("sealed")
        .sealer_mut()
        .set_next_counter_for_test(SEAL_LIMIT);
    assert!(c.send_frame(1000, Bytes::from_static(b"g")).await.is_err());
    assert_drained_then_ended(&mut c, UdpEnd::SealLimit, &[20]).await;
    assert_eq!(c.stats.seal_exhausted, 1, "once, not once per later send");
}

#[tokio::test]
async fn the_integrity_limit_drains_then_ends_at_once() {
    let (mut c, _sink, mut ss) = sealed(ResetToken::from_bytes([0; 16])).await;
    c.process_datagram(&seal(&mut ss, &rel(1, 20, b"a")));
    c.seal
        .opener()
        .expect("sealed")
        .set_forged_for_test(INTEGRITY_LIMIT + 1);
    c.process_datagram(&seal(&mut ss, &rel(2, 21, b"b")));
    assert_drained_then_ended(&mut c, UdpEnd::SealLimit, &[20]).await;
    assert_eq!(c.stats.seal_integrity_limit, 1);
}
