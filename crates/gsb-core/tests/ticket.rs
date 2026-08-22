//! The ticket-validation hook (feature A, item 2) invariants:
//!
//! - the base DEFINES the hook but ships no validator: the tests supply
//!   their own (the platform's adapter is the user's code);
//! - a valid ticket authenticates with the hook's identity (which
//!   supersedes `Auth.name`) and PINS the join to the ticket's room;
//! - an invalid ticket, an empty ticket on a ticket-auth server, and a
//!   timed-out validation are all NORMAL rejections (ERROR code 10):
//!   the connection stays alive (a fresh ticket may be re-presented) and
//!   none of them count against the protocol-violation budget;
//! - a join to a room other than the ticket's is a normal rejection
//!   (ERROR code 11), not a violation;
//! - a slow validator (slower than the hook's timeout) times out cleanly
//!   and the actor stays responsive;
//! - with no hook configured, the legacy local-auth path is unchanged.
//!
//! The actor is driven directly over its inbox (no transport). Most
//! tests use a dead registry (auth and the pin check are local); the
//! "join the pinned room succeeds" test uses a live registry.

use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use gsb_core::auth::{TicketAuth, TicketError, TicketValidator, ValidatedTicket};
use gsb_core::channel::{channel, FrameBatch};
use gsb_core::conn::{ConnIn, ConnectionActor};
use gsb_core::id::{ConnectionId, EntityId, RoomId};
use gsb_core::metrics::MetricsEvent;
use gsb_core::registry::{BuiltRoom, Registry, RegistryMsg, RoomFactory};
use gsb_core::room::{Action, RoomConfig, RoomLogic, TickCtx};
use gsb_core::ticker::Ticker;
use gsb_protocol::base;
use gsb_protocol::{base_table, op};
use prost::Message;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

const WAIT: Duration = Duration::from_secs(5);
const HZ: f64 = 60.0;

fn frame(op: u16, payload: &[u8]) -> gsb_protocol::FrameBody {
    gsb_protocol::FrameBody::new(op, payload.to_vec())
}

fn auth_frame(name: &str, ticket: &[u8]) -> gsb_protocol::FrameBody {
    let a = base::Auth {
        name: name.into(),
        ticket: ticket.to_vec(),
    };
    frame(op::base::AUTH_REQ, &a.encode_to_vec())
}

fn join_frame(room: u64) -> gsb_protocol::FrameBody {
    let j = base::JoinRoom {
        room_id: room,
        // The spawn-point fields are irrelevant to the pin check (the
        // registry is the authority on spawn points).
    };
    frame(op::base::JOIN_ROOM_REQ, &j.encode_to_vec())
}

/// A connection actor with the given ticket hook (`None` = local auth)
/// and a DEAD registry (every registry send fails).
fn spawn_actor(
    conn: u64,
    auth: Option<TicketAuth>,
) -> (
    mpsc::Sender<ConnIn>,
    mpsc::Receiver<FrameBatch>,
    JoinHandle<()>,
) {
    let (inbox_tx, inbox) = channel::<ConnIn>(128);
    let (out_tx, out_rx) = channel::<FrameBatch>(16);
    let (reg_tx, _reg_rx) = channel::<RegistryMsg>(16);
    let (metrics_tx, _metrics_rx) = mpsc::channel::<MetricsEvent>(16);
    let actor = ConnectionActor::new(
        ConnectionId(conn),
        SocketAddr::from(([127, 0, 0, 1], 41_000u16 + conn as u16)),
        Arc::new(base_table()),
        reg_tx,
        inbox,
        out_tx,
        metrics_tx,
        auth,
    );
    let h = tokio::spawn(actor.run());
    (inbox_tx, out_rx, h)
}

/// A validator that accepts exactly `good` and maps it to
/// `(player, room)`; everything else is rejected.
fn validator(good: &'static [u8], player: &'static str, room: u64) -> TicketValidator {
    Arc::new(move |t: bytes::Bytes| {
        Box::pin(async move {
            if t.as_ref() == good {
                Ok(ValidatedTicket {
                    player: player.into(),
                    room: RoomId(room),
                })
            } else {
                Err(TicketError::Rejected("bad ticket".into()))
            }
        }) as Pin<Box<dyn Future<Output = Result<ValidatedTicket, TicketError>> + Send>>
    })
}

async fn read_auth_result(out: &mut mpsc::Receiver<FrameBatch>) -> base::AuthResult {
    let batch = tokio::time::timeout(WAIT, out.recv())
        .await
        .expect("timed out")
        .expect("out closed");
    for f in batch {
        if f.op == op::base::AUTH_RESULT {
            return base::AuthResult::decode(f.payload.as_ref()).expect("AuthResult decode");
        }
    }
    panic!("batch without AUTH_RESULT");
}

async fn read_error(out: &mut mpsc::Receiver<FrameBatch>) -> (u32, String) {
    let batch = tokio::time::timeout(WAIT, out.recv())
        .await
        .expect("timed out")
        .expect("out closed");
    for f in batch {
        if f.op == op::base::ERROR {
            let e = base::Error::decode(f.payload.as_ref()).expect("Error decode");
            return (e.code, e.message);
        }
    }
    panic!("batch without ERROR");
}

/// A valid ticket authenticates with the hook's identity (player + the
/// pinned room), which supersedes `Auth.name`.
#[tokio::test]
async fn valid_ticket_auths_with_identity() {
    let hook = TicketAuth {
        validator: validator(b"good", "neo", 1),
        timeout: Duration::from_millis(200),
    };
    let (in_tx, mut out, handle) = spawn_actor(1, Some(hook));

    // The client's `name` is IGNORED (the hook is the identity
    // authority): only the ticket matters.
    in_tx
        .send(ConnIn::Frame(auth_frame("impersonator", b"good")))
        .await
        .expect("inbox open");
    let r = read_auth_result(&mut out).await;
    assert!(r.ok, "valid ticket must authenticate");
    assert_eq!(r.player, "neo");
    assert_eq!(r.room, 1);
    drop(in_tx);
    handle.await.unwrap();
}

/// An invalid ticket is a NORMAL rejection (code 10): the connection
/// stays alive (a fresh ticket may be re-presented) and it does NOT
/// count against the violation budget (three hard violations after the
/// ticket failure are still ANSWERED — a budgeted ticket failure would
/// have left only room for two).
#[tokio::test]
async fn invalid_ticket_normal_reject_not_budgeted() {
    let hook = TicketAuth {
        validator: validator(b"good", "neo", 1),
        timeout: Duration::from_millis(200),
    };
    let (in_tx, mut out, handle) = spawn_actor(2, Some(hook.clone()));

    // Invalid ticket: code 10 (not 9 — no close, no violation).
    in_tx
        .send(ConnIn::Frame(auth_frame("x", b"bad")))
        .await
        .expect("inbox open");
    let (code, _msg) = read_error(&mut out).await;
    assert_eq!(code, 10, "ticket failure is a normal rejection");

    // The connection is still alive (WaitingAuth): a VALID ticket now
    // succeeds — the failed attempt cost the client nothing but one
    // round trip.
    in_tx
        .send(ConnIn::Frame(auth_frame("x", b"good")))
        .await
        .expect("inbox open");
    let r = read_auth_result(&mut out).await;
    assert!(r.ok);
    assert_eq!(r.player, "neo");

    // Budget isolation: three hard violations (unknown base-band ops,
    // weight 4 each, budget 16) are all ANSWERED. If the ticket failure
    // had counted weight 4, the third would have closed the connection.
    for i in 0..3u16 {
        in_tx
            .send(ConnIn::Frame(frame(42 + i, &[])))
            .await
            .expect("inbox open");
    }
    for _ in 0..3 {
        let (code, _msg) = read_error(&mut out).await;
        assert_ne!(code, 9, "a budgeted ticket failure would close here");
    }
    drop(in_tx);
    handle.await.unwrap();
}

/// An empty ticket on a ticket-auth server: a normal rejection (code
/// 10) — the client must present a ticket; the connection stays alive.
#[tokio::test]
async fn empty_ticket_on_ticket_server_rejected() {
    let hook = TicketAuth {
        validator: validator(b"good", "neo", 1),
        timeout: Duration::from_millis(200),
    };
    let (in_tx, mut out, handle) = spawn_actor(3, Some(hook));
    in_tx
        .send(ConnIn::Frame(auth_frame("x", &[])))
        .await
        .expect("inbox open");
    let (code, _msg) = read_error(&mut out).await;
    assert_eq!(code, 10);
    // Alive: a valid ticket still authenticates.
    in_tx
        .send(ConnIn::Frame(auth_frame("x", b"good")))
        .await
        .expect("inbox open");
    let r = read_auth_result(&mut out).await;
    assert!(r.ok);
    drop(in_tx);
    handle.await.unwrap();
}

/// A slow validator (slower than the hook's timeout) times out
/// (code 10, the worker's timeout is the resource guard) and the actor
/// stays responsive: the NEXT auth attempt is processed at all.
#[tokio::test]
async fn slow_validator_times_out_actor_responsive() {
    // The validator sleeps 500 ms; the hook's timeout is 60 ms.
    let slow: TicketValidator = Arc::new(move |_t: bytes::Bytes| {
        Box::pin(async move {
            tokio::time::sleep(Duration::from_millis(500)).await;
            Ok(ValidatedTicket {
                player: "slow".into(),
                room: RoomId(1),
            })
        })
    });
    let hook = TicketAuth {
        validator: slow,
        timeout: Duration::from_millis(60),
    };
    let (in_tx, mut out, handle) = spawn_actor(4, Some(hook));

    in_tx
        .send(ConnIn::Frame(auth_frame("x", b"whatever")))
        .await
        .expect("inbox open");
    let (code, _msg) = read_error(&mut out).await;
    assert_eq!(code, 10, "a timed-out validation is a normal rejection");

    // Liveness: a SECOND auth attempt is processed and answered too
    // (it times out as well — the hook is fixed for the actor). If the
    // first timeout had parked the actor, this read would time out.
    in_tx
        .send(ConnIn::Frame(auth_frame("x", b"again")))
        .await
        .expect("inbox open");
    let (code2, _msg2) = read_error(&mut out).await;
    assert_eq!(code2, 10);
    drop(in_tx);
    handle.await.unwrap();
}

/// The ticket PINS the room: joining any other room is a normal
/// rejection (code 11); joining the pinned room succeeds (live
/// registry). The pin is enforced by the connection actor BEFORE the
/// registry is consulted (the wrong-room join never reaches it).
#[tokio::test]
async fn ticket_pins_room() {
    // Live registry with the pinned room pre-created.
    let factory: RoomFactory<(), (), ()> = Arc::new(|_id, _config| BuiltRoom::Single {
        world: (),
        logic: Box::new(NoopLogic) as Box<dyn RoomLogic<(), GroupKey = ()>>,
    });
    let (reg_tx, reg_rx) = channel::<RegistryMsg>(4096);
    let (ticker, _ticker_task) = Ticker::spawn(HZ, 64);
    let (metrics_tx, _metrics_rx) = mpsc::channel::<MetricsEvent>(1);
    let reg_handle = tokio::spawn(
        Registry::new(reg_rx, reg_tx.clone(), factory, ticker, metrics_tx, None, None).run(),
    );
    // Create room 1 (the ticket's room).
    {
        let (reply_tx, reply_rx) =
            tokio::sync::oneshot::channel::<Result<gsb_core::registry::RoomStatus, gsb_core::error::CoreError>>();
        reg_tx
            .send(RegistryMsg::CreateRoom {
                config: RoomConfig {
                    id: RoomId(1),
                    tick_hz: HZ,
                    ..Default::default()
                },
                reply: reply_tx,
            })
            .await
            .expect("registry gone");
        tokio::time::timeout(WAIT, reply_rx)
            .await
            .expect("timed out")
            .expect("reply dropped")
            .expect("create failed");
    }

    let hook = TicketAuth {
        validator: validator(b"good", "neo", 1),
        timeout: Duration::from_millis(200),
    };
    // A connection actor bound to the LIVE registry.
    let (inbox_tx, inbox) = channel::<ConnIn>(128);
    let (out_tx, mut out_rx) = channel::<FrameBatch>(16);
    let (metrics_tx2, _metrics_rx2) = mpsc::channel::<MetricsEvent>(16);
    let actor = ConnectionActor::new(
        ConnectionId(5),
        SocketAddr::from(([127, 0, 0, 1], 41_005)),
        Arc::new(base_table()),
        reg_tx.clone(),
        inbox,
        out_tx,
        metrics_tx2,
        Some(hook),
    );
    let handle = tokio::spawn(actor.run());

    // Authenticate (ticket pins room 1).
    inbox_tx
        .send(ConnIn::Frame(auth_frame("x", b"good")))
        .await
        .expect("inbox open");
    let r = read_auth_result(&mut out_rx).await;
    assert!(r.ok);
    assert_eq!(r.room, 1);

    // Join room 2 (NOT the pinned room): code 11, local rejection.
    inbox_tx.send(ConnIn::Frame(join_frame(2))).await.expect("inbox open");
    let (code, _msg) = read_error(&mut out_rx).await;
    assert_eq!(code, 11, "a non-pinned join is a normal rejection");

    // Join room 1 (the pinned room): accepted (the registry spawns).
    inbox_tx.send(ConnIn::Frame(join_frame(1))).await.expect("inbox open");
    let batch = tokio::time::timeout(WAIT, out_rx.recv())
        .await
        .expect("timed out")
        .expect("out closed");
    let joined = batch
        .iter()
        .find(|f| f.op == op::base::JOIN_ROOM_RESULT)
        .map(|f| base::JoinRoomResult::decode(f.payload.as_ref()).expect("decode"));
    assert!(
        joined.is_some(),
        "the pinned join must succeed; batch ops: {:?}",
        batch.iter().map(|f| f.op).collect::<Vec<_>>()
    );
    drop(inbox_tx);
    handle.await.unwrap();
    reg_tx.send(RegistryMsg::Shutdown).await.expect("registry gone");
    drop(reg_tx);
    reg_handle.await.unwrap();
}

/// With NO hook, the legacy local-auth path is unchanged: `Auth.name`
/// is accepted as-is, the result carries no ticket identity (empty
/// player, room 0), and there is no pin (any join is allowed).
#[tokio::test]
async fn no_hook_local_auth_unchanged() {
    let (in_tx, mut out, handle) = spawn_actor(6, None);
    in_tx
        .send(ConnIn::Frame(auth_frame("bob", &[])))
        .await
        .expect("inbox open");
    let r = read_auth_result(&mut out).await;
    assert!(r.ok, "local auth must succeed without a ticket");
    assert_eq!(r.player, "", "local auth carries no hook identity");
    assert_eq!(r.room, 0, "local auth pins no room");
    drop(in_tx);
    handle.await.unwrap();
}

/// A stand-in room logic (joinable; the registry is the test target).
struct NoopLogic;
impl RoomLogic<()> for NoopLogic {
    type GroupKey = ();
    fn snapshot_op(&self) -> u16 {
        0x7F10
    }
    fn private_op(&self) -> u16 {
        0x7F11
    }
    fn group_of(&self, _w: &(), _c: ConnectionId) -> Self::GroupKey {}
    fn snapshot(&mut self, _w: &mut (), _c: &TickCtx, _g: &(), _o: &mut bytes::BytesMut) -> bool {
        false
    }
    fn on_join(&mut self, _w: &mut (), _c: ConnectionId) -> EntityId {
        1
    }
    fn on_leave(&mut self, _w: &mut (), _c: ConnectionId) {}
    fn ingest(&mut self, _w: &mut (), _c: &TickCtx, a: &mut Vec<Action>) {
        a.clear();
    }
    fn update(&mut self, _w: &mut (), _c: &TickCtx) {}
}
