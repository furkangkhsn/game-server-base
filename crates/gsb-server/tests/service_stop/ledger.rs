//! The test game: rooms that settle with a ledger service on shutdown,
//! and optionally a second service that ignores its stop request.

use std::sync::Arc;
use std::time::Duration;

use gsb_core::channel::{Mailbox, channel, post};
use gsb_core::id::{ConnectionId, PlayerId, RoomId};
use gsb_core::registry::{BuiltRoom, RoomFactory};
use gsb_core::room::{Action, Admission, GameLogic, RoomLogic, TickCtx};
use gsb_core::service::Service;
use gsb_core::shard::BorderRecord;
use gsb_protocol::MessageTable;
use gsb_server::{Config, GameModule, RegistryParts, RegistryTask, ServerError};
use tokio::sync::mpsc;

/// How long a room's final settlement blocks its `on_shutdown`: long
/// enough that a service stopped without waiting for the rooms would be
/// gone before the settlement is sent.
const SETTLING: Duration = Duration::from_millis(200);

/// What the test observes, in the order it happened.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    /// Room `n` ran its `on_shutdown`.
    RoomDown(u64),
    /// The ledger served room `n`'s settlement.
    Settled(u64),
    /// The ledger ended on its stop message.
    Stopped,
}

enum LedgerMsg {
    Settle(u64),
    Stop,
}

/// The ledger: a one-mailbox service in the economy's shape, with a stop
/// message (the actor `Shutdown` idiom).
fn ledger(events: mpsc::Sender<Event>) -> (Mailbox<LedgerMsg>, Service) {
    let (tx, mut rx) = channel::<LedgerMsg>(16);
    let task = tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            match msg {
                LedgerMsg::Settle(room) => {
                    let _ = events.try_send(Event::Settled(room));
                }
                LedgerMsg::Stop => break,
            }
        }
        let _ = events.try_send(Event::Stopped);
    });
    let stop = tx.clone();
    (
        tx,
        Service::new("ledger", task, move || post(&stop, LedgerMsg::Stop)),
    )
}

/// A room that settles with the ledger on its way out.
struct Settling {
    room: u64,
    ledger: Mailbox<LedgerMsg>,
    events: mpsc::Sender<Event>,
}

impl GameLogic<()> for Settling {
    type GroupKey = ();
    type Strip = ();
    fn snapshot_op(&self) -> u16 {
        0x7900
    }
    fn private_op(&self) -> u16 {
        0x7901
    }
    fn group_of(&self, _w: &(), _p: PlayerId) -> Self::GroupKey {}
    fn snapshot(
        &mut self,
        _w: &mut (),
        _c: &TickCtx,
        _g: &(),
        _borrowed: &[BorderRecord<()>],
        _o: &mut bytes::BytesMut,
    ) -> bool {
        false
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
    fn on_shutdown(&mut self) {
        std::thread::sleep(SETTLING);
        let _ = self.events.try_send(Event::RoomDown(self.room));
        let _ = self.ledger.try_send(LedgerMsg::Settle(self.room));
    }
}

impl RoomLogic<()> for Settling {}

/// The module: settling rooms, the ledger, and — when `deaf` — a service
/// that never ends.
pub struct LedgerGame {
    events: mpsc::Sender<Event>,
    deaf: bool,
}

impl LedgerGame {
    pub fn new(events: mpsc::Sender<Event>, deaf: bool) -> Self {
        Self { events, deaf }
    }
}

impl GameModule for LedgerGame {
    fn name(&self) -> &'static str {
        "ledger"
    }

    fn configure(&mut self, _raw: &toml::Table, _engine: &Config) -> Result<(), ServerError> {
        Ok(())
    }

    fn register(&self, _table: &mut MessageTable) {}

    fn spawn_registry(&self, mut parts: RegistryParts) -> RegistryTask {
        if self.deaf {
            let task = tokio::spawn(std::future::pending::<()>());
            parts.service(Service::new("deaf", task, || {}));
        }
        let (ledger, service) = ledger(self.events.clone());
        parts.service(service);
        let events = self.events.clone();
        let factory: RoomFactory<(), (), (), ()> =
            Arc::new(move |id: RoomId, _config| BuiltRoom::Single {
                world: (),
                logic: Box::new(Settling {
                    room: id.0,
                    ledger: ledger.clone(),
                    events: events.clone(),
                }) as Box<dyn RoomLogic<(), GroupKey = (), Strip = ()>>,
            });
        parts.spawn(factory)
    }

    fn describe(&self) -> String {
        "ledger: settling rooms and a ledger service".into()
    }
}
