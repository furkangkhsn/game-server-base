//! What a connection actor loses, counted where it is lost (the second
//! count round, "her şeyi saymalıyız"). Each test drives a real
//! `ConnectionActor` whose registry is the TEST (`rig.rs`): it decides
//! whether the connection is seated in a room, how deep its action and
//! outbound channels are, and when the session ends; the samples the
//! actor flushed are summed at the end.
//!
//! - `requests.rs`: the RPC ledger's two connection-side edges (BACKLOG
//!   B55) — a request dropped on a full action channel, a request
//!   received outside any room — each counted alone, and
//!   `actions_dropped` keeping only game actions.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use gsb_core::channel::{FrameBatch, Mailbox, channel};
use gsb_core::conn::{ConnIn, ConnectionActor};
use gsb_core::id::ConnectionId;
use gsb_core::metrics::{ConnSample, MetricsEvent};
use gsb_core::registry::{RegistryMsg, Seat};
use gsb_core::room::Action;
use gsb_protocol::base::Heartbeat;
use gsb_protocol::{FrameBody, MessageTable, base, base_table, op};
use prost::Message;
use tokio::sync::mpsc;

#[path = "conn_counts/requests.rs"]
mod requests;
#[path = "conn_counts/rig.rs"]
mod rig;

const WAIT: Duration = Duration::from_secs(5);
