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
//! - `heartbeats.rs`: the heartbeat throttle's unanswered surplus
//!   (BACKLOG B56), before and after authentication, reaching the
//!   samples.
//! - `bytes.rs`: `bytes_out` exactly (BACKLOG F2) — the bytes of every
//!   control frame the outbound channel took, and none of a refused frame
//!   or a dropped close notice.
//! - `outbound.rs`: the actor's own outbound losses (BACKLOG B57) — a
//!   control frame a closed outbound channel refused is not "sent", and
//!   a best-effort close notice dropped on a full one is counted.
//! - `unprocessed.rs`: what a server-decided end leaves in the inbox
//!   (BACKLOG B60) — the frames behind the verdict, and the frame that
//!   crossed the pre-auth budget, counted by kind.
//! - `samples.rs`: the counting itself (BACKLOG B59) — a sample dropped
//!   on a full metrics channel loses none of its deltas: the next flush
//!   carries them.
//! - `stopped.rs`: what a registry that has already stopped refuses
//!   (BACKLOG F54) — a join its closed mailbox refused is counted once.

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

#[path = "conn_counts/bytes.rs"]
mod bytes;
#[path = "conn_counts/heartbeats.rs"]
mod heartbeats;
#[path = "conn_counts/outbound.rs"]
mod outbound;
#[path = "conn_counts/requests.rs"]
mod requests;
#[path = "conn_counts/rig.rs"]
mod rig;
#[path = "conn_counts/samples.rs"]
mod samples;
#[path = "conn_counts/stopped.rs"]
mod stopped;
#[path = "conn_counts/unprocessed.rs"]
mod unprocessed;

const WAIT: Duration = Duration::from_secs(5);
