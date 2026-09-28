//! `bytes_out`, exactly (BACKLOG F2): the bytes of the control frames
//! this actor's outbound channel TOOK — each frame's body, a 2-byte
//! opcode and its payload, before any transport framing. B57 narrowed it
//! to that: a frame the closed channel refused, or a close notice the
//! full one dropped, is not in it. Checked byte for byte against the
//! frames the channel actually delivered (the smoke test only asked for
//! "more than zero").

use super::*;
use rig::Conn;

/// A frame's counted size: its body on the channel.
fn size(f: &FrameBody) -> u64 {
    2 + f.payload.len() as u64
}

/// The rig's outbound receiver, taken over by the test (so the frames
/// stay readable after `ended` consumed the rig).
fn take_out(c: &mut Conn) -> mpsc::Receiver<FrameBatch> {
    let (_, spare) = channel::<FrameBatch>(1);
    std::mem::replace(&mut c.out, spare)
}

/// The bytes of every frame still on the channel.
fn drained(out: &mut mpsc::Receiver<FrameBatch>) -> u64 {
    let mut n = 0;
    while let Ok(batch) = out.try_recv() {
        n += batch.iter().map(size).sum::<u64>();
    }
    n
}

/// The bytes of the next batch, waited for.
async fn next(out: &mut mpsc::Receiver<FrameBatch>) -> u64 {
    let batch = tokio::time::timeout(WAIT, out.recv())
        .await
        .expect("a frame in time")
        .expect("out open");
    batch.iter().map(size).sum()
}

async fn auth(c: &Conn) {
    let auth = base::Auth {
        name: String::new(),
        ticket: Vec::new(),
        protocol_version: 0,
    };
    c.send(op::base::AUTH_REQ, auth.encode_to_vec()).await;
}

async fn heartbeat(c: &Conn) {
    c.send(op::base::HEARTBEAT, Heartbeat { tick: 1 }.encode_to_vec())
        .await;
}

/// The awaited sends (the AUTH result, the heartbeat ACK) and the stop's
/// best-effort notice, all taken: `bytes_out` is their bytes, to the
/// byte.
#[tokio::test]
async fn bytes_out_is_every_byte_the_channel_took() {
    let mut c = Conn::open(64);
    let mut out = take_out(&mut c);
    auth(&c).await;
    heartbeat(&c).await;
    c.tell(ConnIn::Shutdown).await;
    let sum = c.ended().await;
    let took = drained(&mut out);
    assert_eq!(sum.frames_out, 3, "the AUTH result, the ACK, the notice");
    assert!(took > 3 * 2, "every frame has a payload: {took}");
    assert_eq!(sum.bytes_out, took);
}

/// The writer is gone when the ACK is sent: the refused ACK's bytes are
/// not in `bytes_out` — only the AUTH result's are.
#[tokio::test]
async fn a_refused_frame_is_not_in_bytes_out() {
    let mut c = Conn::open(64);
    let mut out = take_out(&mut c);
    auth(&c).await;
    let took = next(&mut out).await;
    drop(out);
    heartbeat(&c).await;
    let sum = c.ended().await;
    assert_eq!(sum.frames_out_closed, 1, "the refused ACK");
    assert_eq!(sum.bytes_out, took);
}

/// A one-slot queue the client stopped reading holds the ACK when the
/// server stops: the dropped notice's bytes are not in `bytes_out` —
/// the AUTH result's and the ACK's are.
#[tokio::test]
async fn a_dropped_close_notice_is_not_in_bytes_out() {
    let mut c = Conn::open(1);
    let mut out = take_out(&mut c);
    auth(&c).await;
    let mut took = next(&mut out).await;
    heartbeat(&c).await;
    c.tell(ConnIn::Shutdown).await;
    let sum = c.ended().await;
    took += drained(&mut out);
    assert_eq!(sum.close_notices_dropped, 1);
    assert_eq!(sum.frames_out, 2, "the AUTH result and the ACK");
    assert_eq!(sum.bytes_out, took);
}
