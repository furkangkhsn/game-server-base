//! A forward that overtakes the room's end of the membership (BACKLOG
//! B51). When the ROOM ends a membership — the game's kick, the input-idle
//! ceiling, the room closing or retiring — it closes (or drops) the
//! session's action channel first; the connection learns of it only when
//! the registry's notice arrives (`LeftRoom`, `ServerClosed`,
//! `RoomGone`). A frame the connection forwards in between meets the
//! closed channel: the room never sees it, so no room counter can hold it.
//! The connection counts it — RPC requests and game actions apart — and
//! detaches, so the next frame is answered `ERROR 6` as outside any room.
//!
//! The registry here is the TEST: it seats the connection with an action
//! channel the test holds, so the test decides when the room ends the
//! membership and when the notice lands.

use super::*;
use gsb_core::metrics::ConnSample;
use gsb_core::registry::Seat;
use rig::{GAME_OP, table};

/// A connection actor seated in room 1 by the test's registry.
struct Seated {
    inbox: Mailbox<ConnIn>,
    out: mpsc::Receiver<FrameBatch>,
    metrics: mpsc::Receiver<MetricsEvent>,
    actor: tokio::task::JoinHandle<()>,
    /// The room's end of the session's action channel.
    actions: mpsc::Receiver<Action>,
    /// The registry's inbox, kept open so the actor's sends succeed.
    _registry: mpsc::Receiver<RegistryMsg>,
}

impl Seated {
    async fn new() -> Self {
        let (reg_tx, mut registry) = channel::<RegistryMsg>(64);
        let (inbox, inbox_rx) = channel::<ConnIn>(64);
        let (out_tx, out) = channel::<FrameBatch>(64);
        let (metrics_tx, metrics) = mpsc::channel::<MetricsEvent>(64);
        let actor = ConnectionActor::new(
            ConnectionId(1),
            SocketAddr::from(([127, 0, 0, 1], 45_001)),
            Arc::new(table()),
            reg_tx,
            inbox_rx,
            out_tx,
            metrics_tx,
            None,
        );
        let actor = tokio::spawn(actor.run());
        let auth = base::Auth {
            name: String::new(),
            ticket: Vec::new(),
            protocol_version: 0,
        };
        for f in [
            FrameBody::new(op::base::AUTH_REQ, auth.encode_to_vec()),
            FrameBody::new(
                op::base::JOIN_ROOM_REQ,
                base::JoinRoom { room_id: 1 }.encode_to_vec(),
            ),
        ] {
            inbox.send(ConnIn::Frame(f)).await.expect("actor alive");
        }
        let (actions_tx, actions) = channel::<Action>(64);
        loop {
            let msg = tokio::time::timeout(WAIT, registry.recv())
                .await
                .expect("the join in time")
                .expect("actor alive");
            if let RegistryMsg::SpawnPlayer { reply, .. } = msg {
                let seat = Seat {
                    entity: 7,
                    actions: actions_tx,
                    input_rate: None,
                };
                reply.send(Ok(seat)).expect("actor waits");
                break;
            }
        }
        let mut s = Self {
            inbox,
            out,
            metrics,
            actor,
            actions,
            _registry: registry,
        };
        s.until(op::base::JOIN_ROOM_RESULT).await;
        s
    }

    async fn send(&self, op: u16, payload: Vec<u8>) {
        self.inbox
            .send(ConnIn::Frame(FrameBody::new(op, payload)))
            .await
            .expect("actor alive");
    }

    /// Skip frames until one with `op`; return it.
    async fn until(&mut self, op: u16) -> FrameBody {
        loop {
            let batch = tokio::time::timeout(WAIT, self.out.recv())
                .await
                .expect("a frame in time")
                .expect("out open");
            if let Some(f) = batch.into_iter().find(|f| f.op == op) {
                return f;
            }
        }
    }

    async fn next_error(&mut self) -> base::ErrorCode {
        let f = self.until(op::base::ERROR).await;
        base::Error::decode(&f.payload[..]).expect("decodes").code()
    }

    /// Wait for the actor to end; its samples, summed.
    async fn ended(self) -> ConnSample {
        tokio::time::timeout(WAIT, self.actor)
            .await
            .expect("the actor exits")
            .expect("no panic");
        let mut metrics = self.metrics;
        let mut sum: Option<ConnSample> = None;
        while let Ok(ev) = metrics.try_recv() {
            let MetricsEvent::Conn(s) = ev else { continue };
            sum = Some(match sum {
                None => s,
                Some(t) => ConnSample {
                    actions_dropped: t.actions_dropped + s.actions_dropped,
                    actions_dropped_closed: t.actions_dropped_closed + s.actions_dropped_closed,
                    requests_dropped_closed: t.requests_dropped_closed + s.requests_dropped_closed,
                    requests_no_room: t.requests_no_room + s.requests_no_room,
                    ..s
                },
            });
        }
        sum.expect("at least the final sample")
    }
}

fn game_frame() -> Vec<u8> {
    Heartbeat { tick: 1 }.encode_to_vec()
}

/// The room ends the membership (the kick's and the idle ceiling's
/// despawn close the channel) and the client's RPC request overtakes the
/// notice: counted once, as a request; the next one is answered `ERROR 6`
/// and not counted again; the late notice changes nothing.
#[tokio::test]
async fn a_request_that_overtakes_the_end_of_the_membership_is_counted() {
    let mut s = Seated::new().await;
    s.actions.close();

    s.send(op::base::RPC_REQ, vec![1, 2, 3]).await;
    s.send(op::base::RPC_REQ, vec![4, 5, 6]).await;
    assert_eq!(s.next_error().await, base::ErrorCode::NotInRoom);
    assert!(s.actions.try_recv().is_err(), "the room never saw either");
    s.inbox
        .send(ConnIn::LeftRoom { room: RoomId(1) })
        .await
        .expect("actor alive");
    s.inbox
        .send(ConnIn::Closed {
            reason: "test over".into(),
        })
        .await
        .expect("actor alive");

    let sum = s.ended().await;
    assert_eq!(sum.requests_dropped_closed, 1, "the one lost request");
    assert_eq!(
        sum.requests_no_room, 1,
        "the second, answered ERROR 6: in the ledger once, elsewhere (B55)"
    );
    assert_eq!(sum.actions_dropped_closed, 0, "no game action was sent");
    assert_eq!(sum.actions_dropped, 0, "not a full channel");
}

/// The room is gone (its receiver dropped — a close or retire) and a game
/// action overtakes the kick's close notice: counted once, as an action;
/// the notice then closes the session and the count rides its final
/// sample.
#[tokio::test]
async fn an_action_that_overtakes_the_end_of_the_membership_is_counted() {
    let mut s = Seated::new().await;
    let (_, dead) = channel::<Action>(1);
    drop(std::mem::replace(&mut s.actions, dead));

    s.send(GAME_OP, game_frame()).await;
    s.send(GAME_OP, game_frame()).await;
    assert_eq!(s.next_error().await, base::ErrorCode::NotInRoom);
    s.inbox
        .send(ConnIn::ServerClosed {
            cause: ServerClose::Kicked,
            reason: "kicked".into(),
        })
        .await
        .expect("actor alive");

    let sum = s.ended().await;
    assert_eq!(sum.actions_dropped_closed, 1, "the one lost action");
    assert_eq!(sum.requests_dropped_closed, 0, "no request was sent");
    assert_eq!(sum.server_close, Some(ServerClose::Kicked));
}
