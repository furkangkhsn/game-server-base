//! The connection actor's own outbound losses (BACKLOG B57). Its control
//! frames go out on the same bounded channel the room fans out on: an
//! awaited `send_frame` fails only when that channel is CLOSED (the
//! writer is gone), and the best-effort close notice (`try_notice`) can
//! also meet a FULL one. Before B57 `send_frame` counted the frame as
//! sent before trying, and the notice's two failures were not counted.

use super::*;
use gsb_core::conn::ServerClose;
use rig::Conn;

fn heartbeat() -> Vec<u8> {
    Heartbeat { tick: 1 }.encode_to_vec()
}

/// The writer is gone when the first authenticated heartbeat is answered:
/// the ACK is refused, counted as such and not as sent; the session ends
/// as a dead outbound path.
#[tokio::test]
async fn a_control_frame_a_closed_channel_refused_is_not_counted_as_sent() {
    let mut c = Conn::open(64);
    c.auth().await;
    c.writer_gone();
    c.send(op::base::HEARTBEAT, heartbeat()).await;
    let sum = c.ended().await;
    assert_eq!(sum.frames_out, 1, "the AUTH result only");
    assert_eq!(sum.frames_out_closed, 1, "the refused ACK");
    assert_eq!(sum.server_close, Some(ServerClose::OutboundDead));
}

/// A client that stopped reading: its one-slot outbound queue holds the
/// heartbeat ACK when the server stops, so the stop notice is dropped —
/// and counted.
#[tokio::test]
async fn a_close_notice_dropped_on_a_full_queue_is_counted() {
    let mut c = Conn::open(1);
    c.auth().await;
    c.send(op::base::HEARTBEAT, heartbeat()).await;
    c.tell(ConnIn::Shutdown).await;
    let sum = c.ended().await;
    assert_eq!(sum.close_notices_dropped, 1);
    assert_eq!(sum.frames_out, 2, "the AUTH result and the ACK");
    assert_eq!(sum.frames_out_closed, 0);
}

/// The writer is already gone when a room's kick arrives: the notice has
/// nothing to ride — counted with the other refused control frames.
#[tokio::test]
async fn a_close_notice_on_a_closed_queue_is_counted_as_refused() {
    let mut c = Conn::open(64);
    c.auth().await;
    c.writer_gone();
    c.tell(ConnIn::ServerClosed {
        cause: ServerClose::Kicked,
        reason: "kicked".into(),
    })
    .await;
    let sum = c.ended().await;
    assert_eq!(sum.frames_out_closed, 1);
    assert_eq!(sum.close_notices_dropped, 0);
    assert_eq!(sum.frames_out, 1, "the AUTH result only");
}
