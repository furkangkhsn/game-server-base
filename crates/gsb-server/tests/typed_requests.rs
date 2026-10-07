//! Typed requests end to end (round q3, `docs/SERVICES.md` Faz 3): a
//! real server over TCP, a room whose game takes a `.proto`-declared
//! request through the call table, and `gsb_client::requests` on the
//! other side.
//!
//! The request is the demo's `AbilityUse` — its `impl Call` + `impl
//! Request` written by `gsb-build` from `game.proto`'s `DemoRequests`
//! service — so the chain under test is the whole of Faz 3: options →
//! codegen → `calls.accept::<R>()` → `RpcResponse.game_error` → the
//! typed client. The game answers by the request's `x`:
//!
//! | x | the game | the client sees |
//! |---|---|---|
//! | 0 | `Ok(AbilityResult { ok: true })` | `Ok(..)` |
//! | 1 | `Err(DemoError::default())` — ZERO bytes | `Game(DemoError { code: 0 })` |
//! | 2 | `Err(DemoError { code: 7 })` | `Game(DemoError { code: 7 })` |
//! | 3 | takes it, never answers | `Engine("request taken but not answered")` |
//! | 4 | hands it to a service that never answers | `TimedOut` (client clock), then `SessionEnded` at the stop |
//!
//! and a request of an op nobody claims gets the engine's "no handler".

#![cfg(feature = "game-demo")]

use std::sync::Arc;
use std::time::Duration;

use bytes::BytesMut;
use gsb_client::requests::{RequestClient, RequestError};
use gsb_client::session::{self, Credentials};
use gsb_core::calls::{Ask, BootError, CallSpec, Calls, Mail, Route};
use gsb_core::id::{ConnectionId, PlayerId};
use gsb_core::registry::{BuiltRoom, RoomFactory};
use gsb_core::room::{Action, Admission, GameLogic, RoomLogic, TickCtx};
use gsb_demo::game::{AbilityResult, AbilityUse, DemoError};
use gsb_protocol::MessageTable;
use gsb_protocol::base::RpcResponse;
use gsb_protocol::request::{Call, Request};
use gsb_server::{Config, GameModule, RegistryParts, RegistryTask, ServerError};
use prost::Message;

const OP_SNAP: u16 = 0x7160;
const OP_PRIV: u16 = 0x7161;

/// An op no room claims (and no `handle_request` answers).
#[derive(Clone, PartialEq, prost::Message)]
struct Unclaimed {}
impl Call for Unclaimed {
    type Response = Unclaimed;
    type Error = DemoError;
}
impl Request for Unclaimed {
    const OP: u16 = 1999;
}

/// The never-answering service's one call.
#[derive(Clone, Debug)]
struct Hold;
impl Call for Hold {
    type Response = AbilityResult;
    type Error = DemoError;
}
enum HoldMsg {
    // Held, never read: the service never answers (its mailbox is never
    // drained) — the client's own timeout is what this exercises.
    #[expect(dead_code, reason = "the service never drains its mailbox")]
    Hold(Ask<Hold>),
}

struct Game {
    hold: Route<HoldMsg>,
    /// Kept so the service's mailbox stays open (and never drained).
    _inbox: gsb_core::channel::Inbox<Mail<HoldMsg>>,
}

impl GameLogic<()> for Game {
    type GroupKey = ();
    type Strip = ();

    fn snapshot_op(&self) -> u16 {
        OP_SNAP
    }
    fn private_op(&self) -> u16 {
        OP_PRIV
    }
    fn group_of(&self, _w: &(), _p: PlayerId) -> Self::GroupKey {}
    fn snapshot(
        &mut self,
        _w: &mut (),
        _c: &TickCtx,
        _g: &(),
        _b: &[gsb_core::shard::BorderRecord<()>],
        _o: &mut BytesMut,
    ) -> bool {
        false
    }
    /// The answers, each a length-delimited `RpcResponse` (where answers
    /// ride is the game's choice; the client's extractor mirrors it).
    fn private(
        &mut self,
        _w: &mut (),
        _p: PlayerId,
        _g: &(),
        replies: &[gsb_core::rpc::RpcReply],
        out: &mut BytesMut,
    ) -> bool {
        for r in replies {
            let mut buf = Vec::new();
            RpcResponse::from(r)
                .encode_length_delimited(&mut buf)
                .expect("vec grows");
            out.extend_from_slice(&buf);
        }
        !replies.is_empty()
    }
    fn on_join(&mut self, _w: &mut (), c: ConnectionId) -> Admission {
        Admission {
            player: PlayerId(c.0),
            entity: c.0,
        }
    }
    fn on_leave(&mut self, _w: &mut (), _p: PlayerId) {}
    fn ingest(&mut self, _w: &mut (), _c: &TickCtx, a: &mut Vec<Action>) {
        a.clear();
    }
    fn update(&mut self, _w: &mut (), _c: &TickCtx) {}

    fn boot_calls(&mut self, calls: &mut Calls) -> Result<(), BootError> {
        calls.accept::<AbilityUse>()
    }

    fn update_with_calls(&mut self, _w: &mut (), _c: &TickCtx, calls: &mut Calls) {
        for r in calls.take_incoming::<AbilityUse>() {
            match r.body.x {
                0 => calls.reply(
                    r.reply,
                    Ok(AbilityResult {
                        ok: true,
                        reason: String::new(),
                    }),
                ),
                1 => calls.reply(r.reply, Err(DemoError::default())),
                2 => calls.reply(r.reply, Err(DemoError { code: 7 })),
                3 => {} // taken, the ReplyTo dropped unanswered
                _ => {
                    calls
                        .call(CallSpec::new(&self.hold, Hold, HoldMsg::Hold))
                        .answers(r.reply);
                }
            }
        }
    }
}

impl RoomLogic<()> for Game {}

struct Module;

impl GameModule for Module {
    fn name(&self) -> &'static str {
        "typed-requests"
    }
    fn configure(&mut self, _raw: &toml::Table, _engine: &Config) -> Result<(), ServerError> {
        Ok(())
    }
    fn register(&self, _table: &mut MessageTable) {}
    fn spawn_registry(&self, parts: RegistryParts) -> RegistryTask {
        let factory: RoomFactory<(), (), (), ()> = Arc::new(|_id, _config| {
            let (hold, inbox) = Route::new("hold", 16);
            BuiltRoom::Single {
                world: (),
                logic: Box::new(Game {
                    hold,
                    _inbox: inbox,
                }) as Box<dyn RoomLogic<(), GroupKey = (), Strip = ()>>,
            }
        });
        parts.spawn(factory)
    }
    fn describe(&self) -> String {
        "typed-requests: one room taking AbilityUse through the call table".into()
    }
}

fn extract(f: &gsb_protocol::FrameBody) -> Option<Vec<RpcResponse>> {
    if f.op != OP_PRIV {
        return None;
    }
    let mut out = Vec::new();
    let mut buf = &f.payload[..];
    while !buf.is_empty() {
        out.push(RpcResponse::decode_length_delimited(&mut buf).ok()?);
    }
    Some(out)
}

fn ability(x: i32) -> AbilityUse {
    AbilityUse { x, y: 0 }
}

/// Every ending a typed request can have, against one live room.
#[tokio::test]
async fn typed_requests_end_every_way_against_a_live_room() {
    let cfg = Config {
        bind: "127.0.0.1:0".into(),
        room_count: 1,
        ..Default::default()
    };
    let handle = gsb_server::start_game_server(Box::new(Module), cfg)
        .await
        .expect("the module starts");
    let mut conn = gsb_client::connect::tcp(handle.addr)
        .await
        .expect("connect");
    session::auth_and_join(
        &mut conn,
        &Credentials::named("typed-1"),
        1,
        Duration::from_secs(10),
        |_| {},
    )
    .await
    .unwrap_or_else(|e| panic!("join failed: {e}"));
    let mut c = RequestClient::new(conn, Duration::from_secs(10), Box::new(extract), 64);

    // Typed ok.
    let ok = c.request(&ability(0)).await;
    assert_eq!(
        ok,
        Ok(AbilityResult {
            ok: true,
            reason: String::new()
        })
    );
    // A default-valued typed error: zero bytes on the wire, still typed.
    assert_eq!(
        c.request(&ability(1)).await,
        Err(RequestError::Game(DemoError { code: 0 }))
    );
    assert_eq!(
        c.request(&ability(2)).await,
        Err(RequestError::Game(DemoError { code: 7 }))
    );
    // Engine answers: unanswered at tick end; no handler for the op.
    assert_eq!(
        c.request(&ability(3)).await,
        Err(RequestError::Engine(
            gsb_core::rpc::UNANSWERED_REASON.to_owned()
        ))
    );
    assert_eq!(
        c.request(&Unclaimed {}).await,
        Err(RequestError::Engine(gsb_core::rpc::no_handler_reason(1999)))
    );
    // The client's own clock: the service never answers.
    let t = c.send_within(&ability(4), Duration::from_millis(300)).await;
    assert_eq!(c.wait(t).await, Err(RequestError::TimedOut));
    // Forgotten: nothing comes back for it (the server never answers it
    // either — the session's end below must not count it).
    let forgotten = c.send(&ability(4)).await;
    assert!(c.forget(forgotten.id()));
    assert_eq!(c.wait(forgotten).await, Err(RequestError::Forgotten));
    // The session ends while one is in flight: it ends with it (S16).
    let held = c.send(&ability(4)).await;
    let stopper = tokio::spawn(handle.stop());
    assert_eq!(c.wait(held).await, Err(RequestError::SessionEnded));
    assert!(c.is_ended());
    // A request after the end ends at once.
    assert_eq!(
        c.request(&ability(0)).await,
        Err(RequestError::SessionEnded)
    );
    stopper.await.expect("the server stops");

    let n = c.counters();
    assert_eq!((n.sent, n.ok, n.game_errors, n.engine_errors), (9, 1, 2, 2));
    assert_eq!(
        (n.timed_out, n.forgotten, n.session_ended),
        (1, 1, 2),
        "{n:?}"
    );
    assert_eq!(
        (n.answers_late, n.answers_forgotten, n.undecodable),
        (0, 0, 0)
    );
}

/// One op, two claims: the boot refuses the second (the build-time
/// check's runtime twin — two crates' requests can still meet here).
#[test]
fn a_second_claim_of_an_op_is_a_boot_error() {
    #[derive(Clone, PartialEq, prost::Message)]
    struct Impostor {}
    impl Call for Impostor {
        type Response = Impostor;
        type Error = DemoError;
    }
    impl Request for Impostor {
        const OP: u16 = <AbilityUse as Request>::OP;
    }
    let mut calls = Calls::default();
    calls.accept::<AbilityUse>().expect("first claim");
    let err = calls.accept::<Impostor>().expect_err("collision");
    let BootError::OpCollision { op, first, second } = err else {
        panic!("expected a collision: {err:?}")
    };
    assert_eq!(op, gsb_demo::op::ABILITY);
    assert!(first.ends_with("AbilityUse") && second.ends_with("Impostor"));
}
