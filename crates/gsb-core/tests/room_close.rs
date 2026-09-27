//! The room→registry close verb (BACKLOG E6, `docs/RECONNECT.md` §16).
//!
//! - `registry.rs`: the registry's side, driven with the raw
//!   `RegistryMsg::CloseConn` — the connection is told (`ServerClosed`
//!   with the request's cause), the row is settled (a despawned
//!   membership lets go of its slot, a parked one keeps it, a row whose
//!   transport already died is released), and a stale request is a no-op.
//! - `afk.rs`: the input-idle ceiling's action end to end — the default
//!   (`leave_room`) keeps the socket open and sends nothing; `disconnect`
//!   sends ERROR 9 then closes, books `idle_input`, keeps a park
//!   resumable, and frees a sharded member slot.
//! - `leave.rs`: the default action's contract (BACKLOG B40) — the
//!   connection is out of the room and stays open: `ERROR 6` for game
//!   frames, a direct JOIN (resuming a park), and a registry row that
//!   neither holds a despawned slot nor leaks.
//! - `leave_table.rs`: the registry's side of that action, driven with
//!   the raw `RegistryMsg::LeaveConn`; `leave_races.rs`: a join elsewhere
//!   that overtakes a late request, and the connection's stale-notice
//!   guard.
//! - `close_races.rs`: why a close request is re-checked when it leaves
//!   the room (BACKLOG B41) — a park's report that went ahead of it is
//!   dropped, so only the request's `parked` flag decides the row.
//! - `rejoin_races.rs` (rig: `rejoin_rig.rs`): a close request that a
//!   fresh rejoin overtakes still closes the connection (BACKLOG B43) —
//!   for the idle ceiling and for the game's kick.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use gsb_core::channel::{FrameBatch, Mailbox, channel};
use gsb_core::conn::{ConnIn, ConnectionActor, ServerClose};
use gsb_core::error::CoreError;
use gsb_core::id::{ConnectionId, EntityId, PlayerId, RoomId};
use gsb_core::metrics::MetricsEvent;
use gsb_core::registry::{BuiltRoom, CloseRequest, Registry, RegistryMsg, RoomFactory, RoomStatus};
use gsb_core::room::{
    Action, Admission, AfkAction, Detach, ExpireTo, GameLogic, ResumeFound, RoomConfig, RoomLogic,
    TickCtx,
};
use gsb_core::shard::{BorderRecord, Migrating, ShardLogic};
use gsb_core::ticker::Ticker;
use gsb_protocol::base::Heartbeat;
use gsb_protocol::{FrameBody, MessageTable, base, base_table, op};
use prost::Message;
use tokio::sync::{mpsc, oneshot};

#[path = "room_close/afk.rs"]
mod afk;
#[path = "room_close/close_races.rs"]
mod close_races;
#[path = "room_close/leave.rs"]
mod leave;
#[path = "room_close/leave_races.rs"]
mod leave_races;
#[path = "room_close/leave_table.rs"]
mod leave_table;
#[path = "room_close/logic.rs"]
mod logic;
#[path = "room_close/registry.rs"]
mod registry;
#[path = "room_close/rejoin_races.rs"]
mod rejoin_races;
#[path = "room_close/rejoin_rig.rs"]
mod rejoin_rig;
#[path = "room_close/rig.rs"]
mod rig;

const WAIT: Duration = Duration::from_secs(5);

/// A park that outlives every test.
const HOLD: Detach = Detach::Hold {
    grace: Some(Duration::from_secs(3600)),
    to: ExpireTo::Despawn,
};

/// A live registry over `factory`, keeping its metrics receiver.
fn start(
    factory: RoomFactory<(), (), (), ()>,
) -> (Mailbox<RegistryMsg>, mpsc::Receiver<MetricsEvent>) {
    let (tx, rx) = channel::<RegistryMsg>(4096);
    let (ticker, _task) = Ticker::spawn(60.0, 64).expect("valid tick rate");
    let (metrics_tx, metrics) = mpsc::channel::<MetricsEvent>(1024);
    tokio::spawn(
        Registry::new(
            rx,
            tx.clone(),
            factory,
            ticker,
            metrics_tx,
            None,
            None,
            None,
        )
        .run(),
    );
    (tx, metrics)
}

async fn create(tx: &Mailbox<RegistryMsg>, config: RoomConfig) {
    let (reply, rx) = oneshot::channel();
    tx.send(RegistryMsg::CreateRoom { config, reply })
        .await
        .expect("registry alive");
    tokio::time::timeout(WAIT, rx)
        .await
        .expect("in time")
        .expect("reply")
        .expect("created");
}

async fn status(tx: &Mailbox<RegistryMsg>, id: RoomId) -> RoomStatus {
    let (reply, rx) = oneshot::channel();
    tx.send(RegistryMsg::RoomStatus { id, reply })
        .await
        .expect("registry alive");
    tokio::time::timeout(WAIT, rx)
        .await
        .expect("in time")
        .expect("reply")
}
