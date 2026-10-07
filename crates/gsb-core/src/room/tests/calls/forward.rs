//! `forward` (SERVICES.md §5.8, spike `forward`): a shared one-line rule
//! sends a request straight to a service and its answer straight to the
//! client. The game never sees it; the service sees the sender's
//! verified identity; it needs no owner.

use super::*;

fn answer_friends(rig: &mut Rig, names: &[&str]) -> Vec<Option<String>> {
    let inbox = rig.friends.as_mut().expect("a friends service");
    let mut players = Vec::new();
    while let Ok(Mail::Call(Envelope { cx, msg })) = inbox.try_recv() {
        let FriendsMsg::List(ask) = msg;
        players.push(cx.player().map(|p| p.as_str().to_owned()));
        assert_eq!(ask.call.page, 2);
        ask.answer(Ok(FriendList {
            names: names.iter().map(|s| (*s).to_owned()).collect(),
        }));
    }
    players
}

#[test]
fn a_forwarded_request_skips_the_game_and_answers_the_client() {
    let mut rig = Rig::new(RigCfg {
        friends: true,
        mode: Mode {
            // Even an unbound session: a forward rule needs no owner.
            unbound: true,
            ..Default::default()
        },
        ..Default::default()
    });
    let mut a = rig.join(1, "alice");
    a.request(4, 1005, &ListFriends { page: 2 });
    rig.step();
    assert_eq!(
        answer_friends(&mut rig, &["bob", "cem"]),
        [Some("alice".into())]
    );
    assert!(a.answers().is_empty(), "answered at the next RESPONSES");
    rig.step();
    let got = a.answers();
    assert_eq!(got.len(), 1);
    assert_eq!((got[0].id, got[0].op, got[0].ok), (4, 1005, true));
    let list = FriendList::decode(&got[0].payload[..]).expect("FriendList");
    assert_eq!(list.names, ["bob", "cem"]);
    assert!(
        !rig.seen().iter().any(|s| matches!(s, Seen::Request(..))),
        "the game never saw it"
    );
    let c = rig.counts();
    assert_eq!(c.requests_forwarded, 1);
    assert_eq!(c.requests_unbound, 0);
    assert_eq!((c.answers_sent, c.responses_delivered), (1, 0));
    assert_eq!(c.results_ok, 1);
}

/// A forwarded request's id is in flight until it is answered: a re-sent
/// id is a duplicate.
#[test]
fn a_forwarded_id_is_in_flight_until_answered() {
    let mut rig = Rig::new(RigCfg {
        friends: true,
        ..Default::default()
    });
    let mut a = rig.join(1, "alice");
    a.request(4, 1005, &ListFriends { page: 2 });
    rig.step();
    a.request(4, 1005, &ListFriends { page: 2 });
    rig.step();
    assert_eq!(a.answers()[0].reason, crate::rpc::DUPLICATE_REASON);
    answer_friends(&mut rig, &[]);
    rig.step();
    assert!(a.answers()[0].ok);
}

/// `forward` and `accept` of one op is a boot error.
#[test]
fn forward_and_accept_of_one_op_collide() {
    let (route, _inbox) = Route::<FriendsMsg>::new("friends", 4);
    let mut calls = Calls::default();
    calls.accept::<ListFriends>().expect("first claim");
    let err = calls
        .forward(move |r: ListFriends| {
            CallSpec::new(&route, List { page: r.page }, FriendsMsg::List)
        })
        .expect_err("second claim");
    assert!(matches!(err, BootError::OpCollision { op: 1005, .. }));
}
