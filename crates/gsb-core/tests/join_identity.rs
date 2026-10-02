//! K4 (`docs/GAME-MODULE.md`): a sharded room's join router
//! (`BuiltRoom::Sharded::home_shard`) and the room's join hook
//! (`GameLogic::on_join_as`) receive the joiner's AUTHENTICATED identity:
//!
//! - with a ticket hook, the ticket's validated `player` — the name the
//!   client claims in `Auth.name` is ignored;
//! - on the legacy local-auth path, the claimed `Auth.name` (a
//!   development path: nothing authoritative stands behind it);
//! - empty for an anonymous session.
//!
//! Real connection actors over a live registry, so the identity under
//! test is whatever the auth step settled — not a hand-built message.

use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use gsb_core::auth::{TicketAuth, TicketError, ValidatedTicket};
use gsb_core::channel::{FrameBatch, Mailbox, channel};
use gsb_core::conn::{ConnIn, ConnectionActor};
use gsb_core::id::{ConnectionId, RoomId};
use gsb_core::metrics::MetricsEvent;
use gsb_core::registry::{BuiltRoom, Registry, RegistryMsg, RoomFactory};
use gsb_core::room::{RoomConfig, RoomLogic};
use gsb_core::shard::ShardLogic;
use gsb_core::ticker::Ticker;
use gsb_protocol::{base, base_table, op};
use prost::Message;
use tokio::sync::mpsc;

#[path = "join_identity/claims.rs"]
mod claims;
#[path = "join_identity/logic.rs"]
mod logic;

use logic::{IdLogic, SHARDS, SPAN, Seen};

const WAIT: Duration = Duration::from_secs(5);

/// Three shards; the router sends `neo` to shard 1, `bob` to shard 2 and
/// everyone else to shard 0 — and reports every identity it was asked.
fn sharded(seen: Seen) -> RoomFactory<(), (), (), ()> {
    Arc::new(move |_id, _cfg| {
        let shards = (0..SHARDS)
            .map(|index| {
                let logic = IdLogic {
                    index,
                    serial: 0,
                    seen: seen.clone(),
                };
                (
                    (),
                    Box::new(logic)
                        as Box<dyn ShardLogic<(), GroupKey = (), State = (), Strip = ()>>,
                )
            })
            .collect();
        let routed = seen.clone();
        BuiltRoom::Sharded {
            shards,
            home_shard: Arc::new(move |_conn, identity: &str| {
                let shard = match identity {
                    "neo" => 1,
                    "bob" => 2,
                    _ => 0,
                };
                let _ = routed.send(("route", shard, identity.to_string()));
                shard
            }),
        }
    })
}

/// One room actor running [`IdLogic`].
fn single(seen: Seen) -> RoomFactory<(), (), (), ()> {
    Arc::new(move |_id, _cfg| BuiltRoom::Single {
        world: (),
        logic: Box::new(IdLogic {
            index: 0,
            serial: 0,
            seen: seen.clone(),
        }) as Box<dyn RoomLogic<(), GroupKey = (), Strip = ()>>,
    })
}

/// A live registry over `factory` with room 1 created.
async fn registry(factory: RoomFactory<(), (), (), ()>) -> Mailbox<RegistryMsg> {
    let (tx, rx) = channel::<RegistryMsg>(4096);
    let (ticker, _task) = Ticker::spawn(60.0, 64).expect("valid tick rate");
    let (metrics, _m) = mpsc::channel::<MetricsEvent>(64);
    tokio::spawn(Registry::new(rx, tx.clone(), factory, ticker, metrics, None, None, None).run());
    let (reply, created) = tokio::sync::oneshot::channel();
    let config = RoomConfig {
        id: RoomId(1),
        tick_hz: 60.0,
        ..Default::default()
    };
    tx.send(RegistryMsg::CreateRoom { config, reply })
        .await
        .expect("registry alive");
    let created = tokio::time::timeout(WAIT, created).await.expect("in time");
    created.expect("reply").expect("room 1 created");
    tx
}

/// The platform's ticket hook: ticket `t-neo` is player `neo`, pinned to
/// room 1; every other ticket is rejected.
fn hook() -> TicketAuth {
    type Validation = Pin<Box<dyn Future<Output = Result<ValidatedTicket, TicketError>> + Send>>;
    let validator = Arc::new(|t: bytes::Bytes| -> Validation {
        Box::pin(async move {
            match t.as_ref() {
                b"t-neo" => Ok(ValidatedTicket {
                    player: "neo".into(),
                    room: RoomId(1),
                    extra: None,
                }),
                _ => Err(TicketError::Rejected("unknown ticket".into())),
            }
        })
    });
    TicketAuth {
        validator,
        timeout: Duration::from_secs(2),
    }
}

/// Connect `conn` (authenticating with `auth`), send AUTH as `name` with
/// `ticket`, join room 1; returns the wire id and the actor's inbox
/// (keep it: dropping it closes the connection).
async fn login(
    reg: &Mailbox<RegistryMsg>,
    conn: u64,
    auth: Option<TicketAuth>,
    name: &str,
    ticket: &[u8],
) -> (u64, Mailbox<ConnIn>) {
    let (inbox_tx, inbox) = channel::<ConnIn>(128);
    let (out_tx, mut out) = channel::<FrameBatch>(64);
    let (metrics, _m) = mpsc::channel::<MetricsEvent>(64);
    let addr = SocketAddr::from(([127, 0, 0, 1], 42_000 + conn as u16));
    let actor = ConnectionActor::new(
        ConnectionId(conn),
        addr,
        Arc::new(base_table()),
        reg.clone(),
        inbox,
        out_tx,
        metrics,
        auth,
    );
    tokio::spawn(actor.run());
    let a = base::Auth {
        name: name.into(),
        ticket: ticket.to_vec(),
        protocol_version: 0,
    };
    let j = base::JoinRoom { room_id: 1 };
    for (code, body) in [
        (op::base::AUTH_REQ, a.encode_to_vec()),
        (op::base::JOIN_ROOM_REQ, j.encode_to_vec()),
    ] {
        let frame = gsb_protocol::FrameBody::new(code, body);
        inbox_tx.send(ConnIn::Frame(frame)).await.expect("actor");
    }
    loop {
        let batch = tokio::time::timeout(WAIT, out.recv()).await;
        let batch = batch.expect("a reply in time").expect("out open");
        for f in batch {
            assert_ne!(f.op, op::base::ERROR, "{name}: the login failed");
            if f.op == op::base::JOIN_ROOM_RESULT {
                let r = base::JoinRoomResult::decode(f.payload.as_ref()).expect("decode");
                return (r.entity, inbox_tx);
            }
        }
    }
}

/// The next `(who, shard, identity)` the room side reported.
async fn next(
    seen: &mut mpsc::UnboundedReceiver<(&'static str, usize, String)>,
) -> (&'static str, usize, String) {
    tokio::time::timeout(WAIT, seen.recv())
        .await
        .expect("a report in time")
        .expect("reports open")
}

/// Ticket path: the router and the home shard's hook get the ticket's
/// player (`neo`), not the claimed name; legacy path: the claimed name;
/// anonymous: empty. Each join lands on the shard its identity routes to.
#[tokio::test]
async fn a_sharded_room_routes_and_spawns_by_the_authenticated_identity() {
    let (seen, mut rx) = mpsc::unbounded_channel();
    let reg = registry(sharded(seen)).await;
    let cases = [
        (1, Some(hook()), "trinity", &b"t-neo"[..], "neo", 1),
        (2, None, "bob", &b""[..], "bob", 2),
        (3, None, "", &b""[..], "", 0),
    ];
    let mut inboxes = Vec::new();
    for (conn, auth, claimed, ticket, identity, shard) in cases {
        let (entity, inbox) = login(&reg, conn, auth, claimed, ticket).await;
        inboxes.push(inbox);
        assert_eq!(
            entity / SPAN,
            shard as u64,
            "{identity:?} joined shard {shard}"
        );
        assert_eq!(next(&mut rx).await, ("route", shard, identity.to_string()));
        assert_eq!(next(&mut rx).await, ("join", shard, identity.to_string()));
    }
}

/// A single room's join hook gets the same identity (the room actor's
/// fresh-join path, reached through the resume fallback for a named
/// session and through the plain join for an anonymous one).
#[tokio::test]
async fn a_single_room_hands_its_join_hook_the_authenticated_identity() {
    let (seen, mut rx) = mpsc::unbounded_channel();
    let reg = registry(single(seen)).await;
    let (_, _a) = login(&reg, 1, Some(hook()), "trinity", b"t-neo").await;
    assert_eq!(next(&mut rx).await, ("join", 0, "neo".to_string()));
    let (_, _b) = login(&reg, 2, None, "bob", b"").await;
    assert_eq!(next(&mut rx).await, ("join", 0, "bob".to_string()));
    let (_, _c) = login(&reg, 3, None, "", b"").await;
    assert_eq!(next(&mut rx).await, ("join", 0, String::new()));
}
