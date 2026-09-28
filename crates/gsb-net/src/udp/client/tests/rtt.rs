//! The client's reliable band times its frames (BACKLOG B2): a clean
//! frame's ACK is an RTT sample, a re-sent frame's is not (Karn's rule),
//! and each re-send doubles the timer. The clocks are the client's own
//! fields, rewound instead of slept.

use super::*;

/// An ACK for a frame sent once is a sample; the estimate appears.
#[tokio::test]
async fn a_clean_frames_ack_is_a_sample() {
    let (mut c, _sink) = detached().await;
    assert_eq!(c.srtt(), None, "nothing measured yet");
    c.send_frame(gsb_protocol::op::base::ERROR, Bytes::from_static(&[9, 0]))
        .await
        .expect("send");
    c.rel.front_mut().expect("outstanding").sent -= Duration::from_millis(30);
    c.process_datagram(&encode_ack(2));
    let srtt = c.srtt().expect("sampled");
    assert!(srtt >= Duration::from_millis(30), "{srtt:?}");
    assert!(c.rel.is_empty());
}

/// A re-sent frame's ACK is no sample, and the doubled timer stays
/// until a clean frame is answered.
#[tokio::test]
async fn a_re_sent_frames_ack_is_no_sample() {
    let (mut c, _sink) = detached().await;
    c.send_frame(gsb_protocol::op::base::ERROR, Bytes::from_static(&[9, 0]))
        .await
        .expect("send");
    let first = c.rto();
    c.rel.front_mut().expect("outstanding").sent -= first;
    c.retransmit_pass();
    assert_eq!(c.stats.retrans_out, 1);
    assert_eq!(c.rto(), first * 2, "the re-send doubled the timer");
    c.process_datagram(&encode_ack(2));
    assert!(c.rel.is_empty(), "released");
    assert_eq!(c.srtt(), None, "Karn: an ambiguous ACK is no sample");
    assert_eq!(c.rto(), first * 2, "and the backoff stays");
    c.send_frame(gsb_protocol::op::base::ERROR, Bytes::from_static(&[9, 1]))
        .await
        .expect("send");
    c.process_datagram(&encode_ack(3));
    assert!(c.srtt().is_some(), "a clean frame is the first sample");
    assert_eq!(
        c.rto(),
        crate::udp::rel::MIN_RTO,
        "a loopback sample: the floor, backoff gone"
    );
}
