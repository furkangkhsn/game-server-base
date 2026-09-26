//! The schedule, the request frame, and reading the answers out of a
//! private frame.

use super::*;

fn ms(n: u64) -> Duration {
    Duration::from_millis(n)
}

/// Nothing is sent before the join; after it the bursts come one
/// interval apart (from the id's phase), `burst` requests each,
/// numbered on — and a client that fell behind sends one burst, not a
/// catch-up volley.
#[test]
fn bursts_follow_the_join_one_interval_apart() {
    let plan = RpcPlan {
        rate: 20.0,
        burst: 2,
    };
    assert_eq!(plan.interval(), ms(100));
    let mut c = RpcClient::new(plan, 0);
    let t0 = Instant::now();
    assert!(c.due(t0 + ms(500)).is_none(), "no request before the join");
    assert_eq!(c.until_due(t0), None);
    c.joined(t0);
    c.joined(t0 + ms(50)); // a second join does not move the schedule
    assert_eq!(c.until_due(t0), Some(Duration::ZERO), "id 0: no phase");
    let ids = |b: Vec<FrameBody>| -> Vec<u64> {
        b.iter()
            .map(|f| {
                assert_eq!(f.op, gsb_protocol::op::base::RPC_REQ);
                let env = gsb_protocol::base::RpcRequest::decode(&f.payload[..]).expect("env");
                assert_eq!(env.op, u32::from(gsb_demo::op::ECONOMY));
                let buy = gsb_demo::game::BuyItem::decode(&env.payload[..]).expect("buy");
                assert_eq!(buy.kind, ITEM);
                env.id
            })
            .collect()
    };
    assert_eq!(ids(c.due(t0).expect("burst")), vec![1, 2]);
    assert!(c.due(t0 + ms(99)).is_none());
    assert_eq!(c.until_due(t0 + ms(60)), Some(ms(40)));
    assert_eq!(ids(c.due(t0 + ms(100)).expect("burst")), vec![3, 4]);
    // 1 s away from the loop: one burst, and the next a full interval on.
    assert_eq!(ids(c.due(t0 + ms(1_100)).expect("burst")), vec![5, 6]);
    assert!(c.due(t0 + ms(1_150)).is_none());
    assert!(c.due(t0 + ms(1_200)).is_some());
    let t = c.finish(t0 + ms(1_200));
    assert_eq!((t.sent, t.open), (8, 8));
}

/// The phase spreads the first burst by id, within one interval.
#[test]
fn the_first_burst_is_phased_by_id() {
    let plan = RpcPlan {
        rate: 1.0,
        burst: 1,
    };
    let t0 = Instant::now();
    let mut a = RpcClient::new(plan, 1);
    let mut b = RpcClient::new(plan, 2);
    a.joined(t0);
    b.joined(t0);
    assert_eq!(a.until_due(t0), Some(ms(997)));
    assert_eq!(b.until_due(t0), Some(ms(994)));
    assert!(a.due(t0 + ms(996)).is_none());
    assert!(a.due(t0 + ms(997)).is_some());
}

/// The answers are read out of any private frame (field 3, beside an
/// ack or a full, or alone) and counted in the ledger.
#[test]
fn answers_are_read_from_the_private_frame() {
    let answer = |id, ok, reason: &str| gsb_protocol::base::RpcResponse {
        id,
        ok,
        op: u32::from(gsb_demo::op::ECONOMY),
        reason: reason.into(),
        payload: Vec::new(),
    };
    let with_ack = gsb_kit::proto::Private {
        payload: Some(gsb_kit::proto::private::Payload::Ack(
            gsb_kit::proto::InputAck { processed_up_to: 7 },
        )),
        responses: vec![
            answer(1, true, ""),
            answer(2, false, gsb_core::rpc::CONN_CAP_REASON),
        ],
        ..Default::default()
    }
    .encode_to_vec();
    let alone = gsb_kit::proto::Private {
        responses: vec![answer(1, true, "")],
        ..Default::default()
    }
    .encode_to_vec();
    let ack_only = gsb_kit::proto::Private {
        payload: Some(gsb_kit::proto::private::Payload::Ack(
            gsb_kit::proto::InputAck { processed_up_to: 8 },
        )),
        ..Default::default()
    }
    .encode_to_vec();
    assert_eq!(responses(&ack_only), Vec::new());
    assert_eq!(responses(&[0xFF, 0xFF]), Vec::new(), "undecodable: none");

    let t0 = Instant::now();
    let mut c = RpcClient::new(
        RpcPlan {
            rate: 10.0,
            burst: 2,
        },
        0,
    );
    c.joined(t0);
    c.due(t0).expect("burst");
    assert_eq!(c.on_private(&with_ack, t0 + ms(40)), 2);
    assert_eq!(c.on_private(&alone, t0 + ms(50)), 1);
    assert_eq!(c.on_private(&ack_only, t0 + ms(60)), 0);
    let t = c.finish(t0 + ms(70));
    assert_eq!((t.ok, t.conn_cap, t.dup_answers), (1, 1, 1));
    assert_eq!(t.ok_lat_us, vec![40_000]);
}
