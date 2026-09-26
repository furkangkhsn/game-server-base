//! A STALLED joiner (F11): its out channel holds one batch and starts
//! full, so the first batch the fan-out owes it — the one carrying its
//! one-shot full — is dropped; the client starts reading the moment a
//! drop shows in the samples, before the next tick. And the same rig
//! over a SINGLE team room: the room actor's side of the drop signal.

use gsb_core::registry::BuiltRoom;
use gsb_core::room::{Action, RoomLogic};

use super::*;
use crate::team::TeamRoom;

type Single = Box<dyn RoomLogic<World, GroupKey = Team, Strip = ()>>;

/// A joined client whose out channel is still full (not yet reading).
pub(in crate::sharded::tests::team_actors) struct Stalled {
    conn: ConnectionId,
    wire: u64,
    out: Inbox<FrameBatch>,
    actions: Mailbox<Action>,
}

impl Rig {
    /// The rig over one single-world [`TeamRoom`] running [`Front`] —
    /// the same game, clients and barrier as the sharded room, on the
    /// room actor instead of four shard actors.
    pub(in crate::sharded::tests::team_actors) async fn single(delta: bool) -> Self {
        let factory: RoomFactory<World, Team, (), ()> = Arc::new(move |_id, _cfg| {
            let room = TeamRoom::with_game(Front::default(), VisionGrid2::<Position>::new(RADIUS));
            let room = if delta { room.with_delta() } else { room };
            BuiltRoom::Single {
                world: World::new(),
                logic: Box::new(room) as Single,
            }
        });
        let (reg, metrics) = boot(factory).await;
        Self::with(reg, metrics, 1)
    }

    /// Batches the fan-out dropped so far, over every actor.
    pub(in crate::sharded::tests::team_actors) fn dropped(&self) -> u64 {
        self.dropped.iter().sum()
    }

    /// [`Rig::join`], stalled: returns once the fan-out dropped a batch
    /// of it (module docs); the channel stays full until
    /// [`Rig::release`].
    pub(in crate::sharded::tests::team_actors) async fn join_stalled(
        &mut self,
        conn: u64,
        identity: &str,
    ) -> Stalled {
        let (out_tx, out) = channel::<FrameBatch>(1);
        out_tx.try_send(Vec::new()).expect("an empty channel");
        let (reply, joined) = oneshot::channel();
        self.reg
            .send(RegistryMsg::SpawnPlayer {
                conn: ConnectionId(conn),
                room: ROOM,
                out: out_tx,
                identity: identity.to_string(),
                reply,
            })
            .await
            .expect("registry");
        let (wire, actions) = tokio::time::timeout(WAIT, joined)
            .await
            .expect("joined in time")
            .expect("reply")
            .expect("accepted");
        let before = self.dropped();
        loop {
            // A room actor joins in a tick's control phase and ships in
            // the same tick (the drop is in before the reply is read); a
            // shard joins on arrival and ships on the next tick.
            while let Ok(event) = self.metrics.try_recv() {
                self.sample(event);
            }
            if self.dropped() > before {
                break;
            }
            self.step().await;
        }
        Stalled {
            conn: ConnectionId(conn),
            wire,
            out,
            actions,
        }
    }

    /// The stalled client starts reading (before the next tick): its
    /// channel is emptied of the stall and it joins the rig's clients;
    /// returns its index. Its view starts empty.
    pub(in crate::sharded::tests::team_actors) fn release(&mut self, s: Stalled) -> usize {
        let Stalled {
            conn,
            wire,
            mut out,
            actions,
        } = s;
        assert!(out.try_recv().expect("the stall").is_empty());
        assert!(out.try_recv().is_err(), "nothing got through");
        self.clients.push(Client {
            conn,
            wire,
            out,
            actions,
            view: ClientView::default(),
            history: Vec::new(),
            doubled: 0,
        });
        self.clients.len() - 1
    }
}
