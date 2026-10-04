//! The client mirror (SERVICES.md §5), reduced to what the shared trait
//! has to prove: the Rust client core sends a typed request, and its
//! per-frame `poll` returns RESPONSES and PUSHES as two separate event
//! kinds; decoding is typed by the same `Request` / `Event` traits the
//! server uses. Language wrappers sit on exactly this shape (bytes in the
//! FFI `Event`, typed in the wrapper).

use std::collections::HashMap;
use std::marker::PhantomData;

use prost::Message;

use crate::request::{Event, Request};
use crate::wire::{self, Frame};

/// Where a request goes (the proposed `RpcRequest.target`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    Room,
    Hub,
}

/// The typed handle a request returns; decoding a response needs it, so
/// the response type comes from the request that was sent.
#[derive(Debug)]
pub struct Ticket<R> {
    pub id: u64,
    _r: PhantomData<fn() -> R>,
}

/// What `poll` yields: two kinds, never mixed.
#[derive(Debug, Clone, PartialEq)]
pub enum ClientEvent {
    Response(RawResponse),
    Push(wire::Push),
}

/// A correlated answer, still bytes (what the FFI `Event` carries).
#[derive(Debug, Clone, PartialEq)]
pub struct RawResponse {
    pub id: u64,
    pub op: u16,
    pub target: Target,
    pub ok: bool,
    pub game_error: bool,
    pub reason: String,
    pub payload: Vec<u8>,
}

/// How a typed call can fail on the client.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClientCallError<E> {
    /// The game's typed error (`R::Error`).
    Game(E),
    /// The engine answered (timeout, cap, no handler, …): its text.
    Engine(String),
    /// Not this ticket's answer, or bytes that do not decode.
    Mismatch,
}

impl RawResponse {
    pub fn decode<R: Request>(
        &self,
        t: &Ticket<R>,
    ) -> Result<R::Response, ClientCallError<R::Error>> {
        if self.id != t.id || self.op != R::OP {
            return Err(ClientCallError::Mismatch);
        }
        match (self.ok, self.game_error) {
            (true, _) => {
                R::Response::decode(&self.payload[..]).map_err(|_| ClientCallError::Mismatch)
            }
            (false, false) => Err(ClientCallError::Engine(self.reason.clone())),
            (false, true) => match R::Error::decode(&self.payload[..]) {
                Ok(e) => Err(ClientCallError::Game(e)),
                Err(_) => Err(ClientCallError::Mismatch),
            },
        }
    }
}

impl wire::Push {
    pub fn decode<E: Event>(&self) -> Option<E> {
        if self.op != u32::from(E::OP) {
            return None;
        }
        E::decode(&self.payload[..]).ok()
    }
}

/// The client core's session half: one id counter for BOTH targets (so
/// ids never collide on one connection), and the in-flight table that
/// tells a room answer from a hub answer.
#[derive(Debug, Default)]
pub struct ClientCore {
    next_id: u64,
    in_flight: HashMap<u64, Target>,
    /// Encoded `RpcRequest` envelopes waiting to be written (`RPC_REQ`).
    pub outbox: Vec<Vec<u8>>,
}

impl ClientCore {
    pub fn request<R: Request>(&mut self, target: Target, req: &R) -> Ticket<R> {
        self.next_id += 1;
        let id = self.next_id;
        self.in_flight.insert(id, target);
        let env = wire::RpcRequest {
            id,
            op: u32::from(R::OP),
            payload: req.encode_to_vec(),
            target: match target {
                Target::Room => wire::target::ROOM,
                Target::Hub => wire::target::HUB,
            },
        };
        self.outbox.push(env.encode_to_vec());
        Ticket {
            id,
            _r: PhantomData,
        }
    }

    /// Demux one received frame into events (the per-frame poll's core).
    pub fn poll(&mut self, frame: Frame, out: &mut Vec<ClientEvent>) {
        match frame {
            Frame::Private { responses } => {
                for r in responses {
                    self.answer(r, out);
                }
            }
            Frame::RpcResp(r) => self.answer(r, out),
            Frame::Push(p) => out.push(ClientEvent::Push(p)),
        }
    }

    fn answer(&mut self, r: wire::RpcResponse, out: &mut Vec<ClientEvent>) {
        // An unknown id (a malformed-envelope answer under id 0, or a
        // duplicate) still surfaces, as a room answer.
        let target = self.in_flight.remove(&r.id).unwrap_or(Target::Room);
        out.push(ClientEvent::Response(RawResponse {
            id: r.id,
            op: r.op as u16,
            target,
            ok: r.ok,
            game_error: r.game_error,
            reason: r.reason,
            payload: r.payload,
        }));
    }
}
