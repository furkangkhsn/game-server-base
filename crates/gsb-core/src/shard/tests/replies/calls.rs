//! The call table on the SHARD actor (round q1): the same phases, the
//! same shared code as the room's (`crate::room::Parts`) — a request the
//! table claims is taken in `update_seam_with_calls`, a call `.answers`
//! it, the answer rides the next tick's RESPONSES.

use super::*;
use crate::calls::{Ask, CallSpec, Calls, Envelope, Mail, Route};

#[derive(Clone, PartialEq, prost::Message)]
pub(super) struct Echo {
    #[prost(uint32, tag = "1")]
    pub n: u32,
}
impl crate::calls::Call for Echo {
    type Response = Echo;
    type Error = Echo;
}
impl crate::calls::Request for Echo {
    const OP: u16 = 0x0500;
}

pub(super) enum EchoMsg {
    Echo(Ask<Echo>),
}

/// The game's one system: every Echo request becomes an echo call that
/// answers it.
pub(super) fn system(echo: &Route<EchoMsg>, calls: &mut Calls) {
    for r in calls.take_incoming::<Echo>() {
        calls
            .call(CallSpec::new(echo, r.body.clone(), EchoMsg::Echo))
            .answers(r.reply);
    }
}

#[tokio::test]
async fn a_shard_takes_a_request_calls_and_answers_through_its_table() {
    let (route, mut inbox) = Route::<EchoMsg>::new("echo", 4);
    let mut a = shard_with(4, 16, Some(route));
    let (_wire, tx, mut rx) = join(&mut a, 1, 8);
    let env = gsb_protocol::base::RpcRequest {
        id: 9,
        op: u32::from(<Echo as crate::calls::Request>::OP),
        payload: Echo { n: 3 }.encode_to_vec(),
    };
    tx.try_send(Action {
        conn: ConnectionId(1),
        player: PlayerId(0),
        op: RPC_REQ_OP,
        payload: env.encode_to_vec().into(),
    })
    .expect("input has room");
    assert!(a.step(&tinfo(1)));
    let Ok(Mail::Call(Envelope { msg, .. })) = inbox.try_recv() else {
        panic!("the call left at FLUSH");
    };
    let EchoMsg::Echo(ask) = msg;
    let n = ask.call.n;
    ask.answer(Ok(Echo { n }));
    assert_eq!(
        drain(&mut rx),
        [Vec::<u64>::new()],
        "not in the issuing tick"
    );
    assert!(a.step(&tinfo(2)));
    assert_eq!(drain(&mut rx), [vec![9]], "answered at RESPONSES");
    let c = *a.calls.counts();
    assert_eq!((c.calls_sent, c.results_ok, c.answers_sent), (1, 1, 1));
    assert_eq!(a.sample().calls, c, "the shard's sample carries the counts");
}
