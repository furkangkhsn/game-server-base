//! The provided [`GameLogic::on_disconnect_with`] is the old hook: a
//! logic that only knows `on_disconnect` answers every cause with it,
//! unchanged — player, identity and answer passed straight through.

use crate::id::{ConnectionId, PlayerId};
use crate::room::{Action, Admission, Detach, DisconnectCause, ExpireTo, GameLogic, TickCtx};

mod refine;

/// Overrides `on_disconnect` only, and logs what it was asked.
struct OldHook {
    asked: Vec<(PlayerId, String)>,
    answer: Detach,
}

impl GameLogic<()> for OldHook {
    type GroupKey = ();
    type Strip = ();
    fn snapshot_op(&self) -> u16 {
        0x7c00
    }
    fn private_op(&self) -> u16 {
        0x7c01
    }
    fn group_of(&self, _w: &(), _p: PlayerId) -> Self::GroupKey {}
    fn snapshot(
        &mut self,
        _w: &mut (),
        _c: &TickCtx,
        _g: &(),
        _b: &[crate::shard::BorderRecord<()>],
        _o: &mut bytes::BytesMut,
    ) -> bool {
        false
    }
    fn on_join(&mut self, _w: &mut (), conn: ConnectionId) -> Admission {
        Admission {
            player: PlayerId(conn.0),
            entity: conn.0,
        }
    }
    fn on_leave(&mut self, _w: &mut (), _p: PlayerId) {}
    fn on_disconnect(&mut self, _w: &mut (), player: PlayerId, identity: &str) -> Detach {
        self.asked.push((player, identity.to_string()));
        self.answer
    }
    fn ingest(&mut self, _w: &mut (), _c: &TickCtx, a: &mut Vec<Action>) {
        a.clear();
    }
    fn update(&mut self, _w: &mut (), _c: &TickCtx) {}
}

#[test]
fn the_default_answers_every_cause_with_on_disconnect() {
    let hold = Detach::Hold {
        grace: None,
        to: ExpireTo::AiHandover,
    };
    let causes = [
        DisconnectCause::ConnectionClosed,
        DisconnectCause::IdleInput,
        DisconnectCause::Kicked,
    ];
    for answer in [Detach::Despawn, hold] {
        let mut logic = OldHook {
            asked: Vec::new(),
            answer,
        };
        for (i, cause) in causes.into_iter().enumerate() {
            let player = PlayerId(i as u64 + 1);
            assert_eq!(
                logic.on_disconnect_with(&mut (), player, "ana", cause),
                answer,
                "{cause:?}: the old hook's answer"
            );
        }
        assert_eq!(
            logic.asked,
            (1..=3)
                .map(|p| (PlayerId(p), "ana".to_string()))
                .collect::<Vec<_>>(),
            "once per end, with the player and the resume key"
        );
    }
}
