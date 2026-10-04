//! EVENTS: what the system pushes to a player identity (SERVICES.md §6).
//!
//! One route for both destinations: identity → the identity's live
//! session (the registry's job), then the connection either writes a
//! `PUSH` frame (`Dest::Client` — the room never sees it) or forwards the
//! push into its CURRENT room membership's inbound channel
//! (`Dest::World` — the channel that already follows the session across
//! resume and shard migration), where READ diverts it to the EVENTS phase.
//!
//! The spike collapses the two actor hops (registry → connection actor)
//! into [`Directory::route`]; each hop's failure keeps its own counter.

use std::collections::HashMap;

use tokio::sync::mpsc;

use crate::request::{ConnId, Event};
use crate::room::Inbound;
use crate::wire::{self, Frame};

/// Where a push goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dest {
    /// Straight to the client (chat from another room, friend online).
    Client,
    /// To the room logic hosting the player (a web-shop credit the world
    /// must apply).
    World,
}

/// A push addressed to a player identity (the verified ticket identity).
#[derive(Debug, Clone)]
pub struct Push {
    pub identity: String,
    pub dest: Dest,
    pub op: u16,
    pub payload: Vec<u8>,
}

impl Push {
    pub fn new<E: Event>(identity: impl Into<String>, dest: Dest, event: &E) -> Self {
        Self {
            identity: identity.into(),
            dest,
            op: E::OP,
            payload: event.encode_to_vec(),
        }
    }
}

/// A `Dest::World` push as the room logic receives it in EVENTS.
#[derive(Debug, Clone)]
pub struct PushIn {
    pub conn: ConnId,
    pub op: u16,
    pub payload: Vec<u8>,
}

impl PushIn {
    /// Typed read: `None` when the op is another event's, or the payload
    /// does not decode (the room counts the latter — see `Room`).
    pub fn decode<E: Event>(&self) -> Option<E> {
        if self.op != E::OP {
            return None;
        }
        E::decode(&self.payload[..]).ok()
    }
}

/// What the registry knows about one identity's live session.
#[derive(Debug, Clone)]
pub struct SessionRoute {
    pub conn: ConnId,
    /// The client declared it understands `PUSH` frames (proposed
    /// `Auth.features`); an older client is never sent one.
    pub push_capable: bool,
    /// The connection's own outbound path (its writer).
    pub client_out: mpsc::Sender<Frame>,
    /// The current room membership's inbound channel, if in a room.
    pub room_in: Option<mpsc::Sender<Inbound>>,
}

/// One counter per way a push ends (SERVICES.md §8).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct PushCounters {
    pub pushes_delivered_client: u64,
    pub pushes_delivered_world: u64,
    pub pushes_offline: u64,
    pub pushes_unsupported: u64,
    pub pushes_client_dropped_full: u64,
    pub pushes_client_dropped_closed: u64,
    pub pushes_world_no_room: u64,
    pub pushes_world_dropped_full: u64,
    pub pushes_world_dropped_closed: u64,
}

/// The registry's identity → session table, modeled.
#[derive(Debug, Default)]
pub struct Directory {
    sessions: HashMap<String, SessionRoute>,
    pub counters: PushCounters,
}

impl Directory {
    pub fn bind(&mut self, identity: impl Into<String>, route: SessionRoute) {
        self.sessions.insert(identity.into(), route);
    }

    pub fn unbind(&mut self, identity: &str) {
        self.sessions.remove(identity);
    }

    /// Route one push. Never waits; every non-delivery is counted under
    /// its own name.
    pub fn route(&mut self, push: Push) {
        let Some(s) = self.sessions.get(&push.identity) else {
            self.counters.pushes_offline += 1;
            return;
        };
        match push.dest {
            Dest::Client => {
                if !s.push_capable {
                    self.counters.pushes_unsupported += 1;
                    return;
                }
                let frame = Frame::Push(wire::Push {
                    op: u32::from(push.op),
                    payload: push.payload,
                });
                match s.client_out.try_send(frame) {
                    Ok(()) => self.counters.pushes_delivered_client += 1,
                    Err(mpsc::error::TrySendError::Full(_)) => {
                        self.counters.pushes_client_dropped_full += 1
                    }
                    Err(mpsc::error::TrySendError::Closed(_)) => {
                        self.counters.pushes_client_dropped_closed += 1
                    }
                }
            }
            Dest::World => {
                let Some(room_in) = &s.room_in else {
                    self.counters.pushes_world_no_room += 1;
                    return;
                };
                let item = Inbound::Push(PushIn {
                    conn: s.conn,
                    op: push.op,
                    payload: push.payload,
                });
                match room_in.try_send(item) {
                    Ok(()) => self.counters.pushes_delivered_world += 1,
                    Err(mpsc::error::TrySendError::Full(_)) => {
                        self.counters.pushes_world_dropped_full += 1
                    }
                    Err(mpsc::error::TrySendError::Closed(_)) => {
                        self.counters.pushes_world_dropped_closed += 1
                    }
                }
            }
        }
    }
}
