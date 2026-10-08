//! The sans-IO table on its own: every ending, every counted loss.

use super::*;
use gsb_protocol::request::Call;

#[derive(Clone, PartialEq, prost::Message)]
struct Buy {
    #[prost(uint32, tag = "1")]
    item: u32,
}
#[derive(Clone, PartialEq, prost::Message)]
struct Bought {
    #[prost(uint32, tag = "1")]
    price: u32,
}
#[derive(Clone, PartialEq, prost::Message)]
struct Refused {
    #[prost(uint32, tag = "1")]
    code: u32,
}
impl Call for Buy {
    type Response = Bought;
    type Error = Refused;
}
impl Request for Buy {
    const OP: u16 = 1234;
}

const T: Duration = Duration::from_secs(2);

fn answer(id: u64) -> RpcResponse {
    RpcResponse {
        id,
        op: u32::from(Buy::OP),
        ..Default::default()
    }
}

/// The frame is the base envelope: `RPC_REQ`, id from 1, the op, the
/// body's bytes.
#[test]
fn start_builds_the_envelope() {
    let mut r = Requests::new(T);
    let (t, f) = r.start(&Buy { item: 7 }, Instant::now());
    assert_eq!((t.id(), f.op), (1, gsb_protocol::op::base::RPC_REQ));
    let env = RpcRequest::decode(&f.payload[..]).expect("envelope");
    assert_eq!((env.id, env.op), (1, 1234));
    assert_eq!(env.payload, Buy { item: 7 }.encode_to_vec());
    let (t2, _) = r.start(&Buy { item: 7 }, Instant::now());
    assert_eq!(t2.id(), 2);
}

/// ok, typed game error (zero bytes too), engine answer: each typed.
#[test]
fn answers_are_typed() {
    let now = Instant::now();
    let mut r = Requests::new(T);
    let (ok, _) = r.start(&Buy { item: 1 }, now);
    let (zero, _) = r.start(&Buy { item: 2 }, now);
    let (coded, _) = r.start(&Buy { item: 3 }, now);
    let (engine, _) = r.start(&Buy { item: 4 }, now);
    assert!(r.take(&ok).is_none(), "still in flight");
    r.answer(RpcResponse {
        ok: true,
        payload: Bought { price: 5 }.encode_to_vec(),
        ..answer(ok.id())
    });
    r.answer(RpcResponse {
        game_error: true,
        ..answer(zero.id())
    });
    r.answer(RpcResponse {
        game_error: true,
        payload: Refused { code: 9 }.encode_to_vec(),
        ..answer(coded.id())
    });
    r.answer(RpcResponse {
        reason: "no request handler for op 0x4d2".into(),
        ..answer(engine.id())
    });
    assert_eq!(r.take(&ok), Some(Ok(Bought { price: 5 })));
    assert_eq!(
        r.take(&zero),
        Some(Err(RequestError::Game(Refused { code: 0 })))
    );
    assert_eq!(
        r.take(&coded),
        Some(Err(RequestError::Game(Refused { code: 9 })))
    );
    assert_eq!(
        r.take(&engine),
        Some(Err(RequestError::Engine(
            "no request handler for op 0x4d2".into()
        )))
    );
    assert!(r.take(&ok).is_none(), "an outcome is taken once");
    let c = r.counters();
    assert_eq!((c.sent, c.ok, c.game_errors, c.engine_errors), (4, 1, 2, 1));
    assert_eq!(r.in_flight(), 0);
}

/// The client's clock times a request out; its late answer is counted
/// and dropped.
#[test]
fn a_request_times_out_and_its_late_answer_is_counted() {
    let now = Instant::now();
    let mut r = Requests::new(T);
    let (t, _) = r.start(&Buy { item: 1 }, now);
    assert_eq!(r.next_due(), Some(now + T));
    r.sweep(now + T - Duration::from_millis(1));
    assert!(r.take(&t).is_none());
    r.sweep(now + T);
    assert_eq!(r.take(&t), Some(Err(RequestError::TimedOut)));
    r.answer(answer(t.id()));
    let c = r.counters();
    assert_eq!((c.timed_out, c.answers_late, c.ok), (1, 1, 0));
}

/// S16: the session's end closes everything in flight.
#[test]
fn the_session_end_closes_every_request() {
    let now = Instant::now();
    let mut r = Requests::new(T);
    let (a, _) = r.start(&Buy { item: 1 }, now);
    let (b, _) = r.start(&Buy { item: 2 }, now);
    r.end_session();
    assert_eq!(r.take(&a), Some(Err(RequestError::SessionEnded)));
    assert_eq!(r.take(&b), Some(Err(RequestError::SessionEnded)));
    assert_eq!(r.counters().session_ended, 2);
    assert_eq!(r.in_flight(), 0);
}

/// A forgotten request yields nothing; its answer is dropped and
/// counted; it neither times out nor counts as cut by the session end.
#[test]
fn a_forgotten_request_drops_its_late_answer() {
    let now = Instant::now();
    let mut r = Requests::new(T);
    let (a, _) = r.start(&Buy { item: 1 }, now);
    let (b, _) = r.start(&Buy { item: 2 }, now);
    let (c, _) = r.start(&Buy { item: 3 }, now);
    assert!(r.forget(a.id()) && r.forget(b.id()) && r.forget(c.id()));
    assert!(!r.forget(a.id()), "forgotten once");
    assert!(!r.is_open(a.id()));
    r.answer(RpcResponse {
        ok: true,
        ..answer(a.id())
    });
    assert!(r.take(&a).is_none());
    r.sweep(now + T);
    assert!(
        r.take(&b).is_none(),
        "a forgotten request does not time out"
    );
    r.end_session();
    assert!(r.take(&c).is_none());
    let n = r.counters();
    assert_eq!(
        (
            n.forgotten,
            n.answers_forgotten,
            n.ok,
            n.timed_out,
            n.session_ended
        ),
        (3, 1, 0, 0, 0)
    );
    assert!(!r.forget(99), "no such request");
}

/// A payload that does not decode as the declared type is its own end.
#[test]
fn an_undecodable_answer_is_counted() {
    let mut r = Requests::new(T);
    let (t, _) = r.start(&Buy { item: 1 }, Instant::now());
    r.answer(RpcResponse {
        ok: true,
        payload: vec![0xFF],
        ..answer(t.id())
    });
    assert_eq!(r.take(&t), Some(Err(RequestError::Undecodable)));
    assert_eq!(r.counters().undecodable, 1);
}

/// The runner with an event table: `PUSH` frames go to the table, an
/// answer to its request, anything else to the frame queue — in one
/// reader, read one bounded turn at a time; the session's end ends it.
#[tokio::test]
async fn the_runner_feeds_pushes_to_its_event_table() {
    use crate::events::Events;
    use crate::frame::FrameTx;
    use gsb_protocol::event::{ClientEvent, Dest, Event};

    #[derive(Clone, PartialEq, prost::Message)]
    struct Line {
        #[prost(string, tag = "1")]
        text: String,
    }
    impl Event for Line {
        const OP: u16 = 2002;
        const TO: Dest = Dest::Client;
    }
    impl ClientEvent for Line {}

    let (a, b) = tokio::io::duplex(1 << 16);
    let (c, d) = tokio::io::duplex(1 << 16);
    let conn = Conn::halves(Box::new(a), Box::new(c));
    let mut tx = FrameTx::new(b);
    drop(d);
    let extract: Extract = Box::new(|_| None);
    let mut client = RequestClient::new(conn, T, extract, 8).with_events(Events::new());
    let push = |seq: u64| gsb_protocol::base::Push {
        publisher: 1,
        seq,
        op: u32::from(Line::OP),
        body: Line { text: "hi".into() }.encode_to_vec(),
        stream: 1,
    };
    tx.send(gsb_protocol::op::base::PUSH, &push(1).encode_to_vec())
        .await
        .unwrap();
    tx.send(gsb_protocol::op::base::HEARTBEAT_ACK, &[])
        .await
        .unwrap();
    tx.send(gsb_protocol::op::base::PUSH, &push(3).encode_to_vec())
        .await
        .unwrap();
    for _ in 0..3 {
        assert!(client.poll(Duration::from_secs(5)).await);
    }
    let events = client.events().expect("a table");
    assert_eq!(events.take::<Line>().len(), 2);
    assert_eq!(events.take_gaps()[0].missed, 1);
    assert_eq!(client.take_frames().count(), 1, "the other frame");
    drop(tx);
    assert!(!client.poll(Duration::from_secs(5)).await, "the end");
    assert!(client.is_ended());
}
