//! The ceiling's verdicts at the server's stop (BACKLOG F56). A room
//! hands its verdicts to the registry with `try_send`: a FULL mailbox
//! keeps them for the next tick, a CLOSED one — the registry has stopped
//! — drops them. Before F56 both ends of that at the stop were
//! uncounted (B57): the refused verdict, and the one still queued when
//! the room itself stopped. Now each is a lost verdict, counted by kind
//! (a close by its reason) and sent once, as the room stops, ahead of
//! its final sample.

use super::*;
use crate::conn::ServerClose;
use crate::metrics::VerdictsLost;
use crate::room::AfkAction;

fn afk(id: u64, action: AfkAction) -> RoomConfig {
    RoomConfig {
        afk_action: action,
        ..cfg(id, Some(5))
    }
}

/// Run `r` past its ceiling with a registry mailbox that is closed (`cap`
/// = 0) or holds one message already (`cap` = 1, full), then stop it;
/// the `VerdictsLost` events it sent, and whether its final sample came
/// last.
fn ceiling_then_stop(r: &mut Rig, cap: usize) -> (Vec<VerdictsLost>, bool) {
    let (tx, rx) = channel(1);
    if cap == 0 {
        drop(rx);
        r.actor.registry = Some(tx);
        r.run_past_the_ceiling();
    } else {
        tx.try_send(crate::registry::RegistryMsg::Shutdown)
            .expect("fills the one slot");
        r.actor.registry = Some(tx);
        r.run_past_the_ceiling();
        drop(rx);
    }
    let (metrics, mut events) = mpsc::channel(8);
    r.actor.metrics = metrics;
    r.actor.finish();
    let mut lost = Vec::new();
    let mut last_is_final = false;
    while let Ok(ev) = events.try_recv() {
        last_is_final = matches!(ev, MetricsEvent::RoomFinal(_));
        if let MetricsEvent::VerdictsLost(v) = ev {
            lost.push(v);
        }
    }
    (lost, last_is_final)
}

impl Rig {
    fn run_past_the_ceiling(&mut self) {
        let _ = self.join(ConnectionId(1), "ana");
        self.step_at(1, 3);
        self.step_at(2, 30);
        assert_eq!(self.disconnects().len(), 1, "the ceiling fired");
    }
}

/// `Disconnect` with the registry already stopped: the close request is
/// refused, dropped — and counted, by its reason.
#[test]
fn a_close_the_stopped_registry_refused_is_a_lost_verdict() {
    let mut r = Rig::new(afk(80, AfkAction::Disconnect), Detach::Despawn);
    let (lost, last_is_final) = ceiling_then_stop(&mut r, 0);
    let [v] = lost[..] else {
        panic!("one VerdictsLost: {lost:?}");
    };
    assert_eq!(v.closes.get(ServerClose::IdleInput), 1);
    assert_eq!(v.closes.total(), 1);
    // The despawn's own report waits for the next tick's flush (phase
    // 0c): the stop finds it still queued.
    assert_eq!(v.detach_despawns, 1);
    assert!(r.actor.close_requests.is_empty(), "not retried");
    assert!(last_is_final, "ahead of the final sample");
}

/// `Disconnect` with the registry's mailbox full until the room stops:
/// the request waits, and the stop counts it.
#[test]
fn a_close_still_queued_at_the_stop_is_a_lost_verdict() {
    let mut r = Rig::new(afk(81, AfkAction::Disconnect), Detach::Despawn);
    let (lost, _) = ceiling_then_stop(&mut r, 1);
    let [v] = lost[..] else {
        panic!("one VerdictsLost: {lost:?}");
    };
    assert_eq!(v.closes.get(ServerClose::IdleInput), 1);
    assert!(r.actor.close_requests.is_empty(), "the stop took it");
}

/// The default `LeaveRoom` with the registry stopped: the leave request
/// is the lost verdict.
#[test]
fn a_leave_the_stopped_registry_refused_is_a_lost_verdict() {
    let mut r = Rig::new(afk(82, AfkAction::LeaveRoom), Detach::Despawn);
    let (lost, _) = ceiling_then_stop(&mut r, 0);
    let [v] = lost[..] else {
        panic!("one VerdictsLost: {lost:?}");
    };
    assert_eq!((v.closes.total(), v.leaves, v.detach_despawns), (0, 1, 0));
}

/// A registry that took every verdict (the despawn's report too, on the
/// tick after): nothing lost, no event.
#[test]
fn a_room_that_lost_no_verdict_sends_none() {
    let mut r = Rig::new(afk(83, AfkAction::Disconnect), Detach::Despawn);
    let (tx, _rx) = channel(8);
    r.actor.registry = Some(tx);
    r.run_past_the_ceiling();
    r.step_at(3, 31);
    let (metrics, mut events) = mpsc::channel(8);
    r.actor.metrics = metrics;
    r.actor.finish();
    while let Ok(ev) = events.try_recv() {
        assert!(!matches!(ev, MetricsEvent::VerdictsLost(_)), "{ev:?}");
    }
}
