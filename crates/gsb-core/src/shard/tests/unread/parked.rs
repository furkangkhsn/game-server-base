//! A resume queued for the identity a stopping shard holds parked
//! (BACKLOG B75): the shard counts it (`resumes_unprocessed`) and answers
//! it `RoomGone` — "counted here" — so the dispatcher's fan-out does not
//! fall back to a fresh join that would be refused and counted again. A
//! resume for an identity parked elsewhere (or nowhere) is dropped
//! unanswered, uncounted here: the fan-out's fallback counts it once.

use super::*;
use crate::error::CoreError;

#[tokio::test]
async fn a_stopping_shard_answers_the_resume_of_its_own_park() {
    use crate::shard::link::InProcLink;

    let mut a = bare_shard(1);
    let (metrics, mut samples) = mpsc::channel(8);
    a.metrics = metrics;
    let (reply_tx, reply_rx) = oneshot::channel();
    let (out_tx, _out_rx) = mpsc::channel::<FrameBatch>(8);
    assert!(a.handle_msg(
        ShardMsg::Join {
            conn: ConnectionId(1),
            epoch: 1,
            identity: "ada".to_string(),
            out: out_tx.clone(),
            reply: reply_tx
        },
        1
    ));
    let _seat = reply_rx.await.expect("delivered").expect("admitted");
    // Parked, as a detach under a hold policy leaves the row.
    let player = a.binding[&ConnectionId(1)];
    a.conns.get_mut(&player).expect("the row").detached = true;

    let (inbox_tx, inbox_rx) = channel::<ShardMsg<TState, TStrip>>(8);
    a.inbox = Box::new(InProcLink::inbound(inbox_rx));
    let (here_tx, here) = oneshot::channel();
    let (elsewhere_tx, elsewhere) = oneshot::channel();
    for (conn, identity, reply) in [(2, "ada", here_tx), (3, "ghost", elsewhere_tx)] {
        inbox_tx
            .try_send(ShardMsg::Resume {
                conn: ConnectionId(conn),
                epoch: 2,
                identity: identity.to_string(),
                out: out_tx.clone(),
                reply,
            })
            .expect("room in the inbox");
    }

    a.finish();

    let mut last = None;
    while let Ok(ev) = samples.try_recv() {
        last = Some(ev);
    }
    let Some(MetricsEvent::RoomFinal(s)) = last else {
        panic!("the final sample: {last:?}");
    };
    assert_eq!(s.stop.resumes_unprocessed, 1, "its own park only");
    assert!(
        matches!(here.await, Ok(Err(CoreError::RoomGone))),
        "the park's resume is answered RoomGone: counted here"
    );
    assert!(
        elsewhere.await.is_err(),
        "another shard's (or no one's) resume is dropped unanswered"
    );
}
