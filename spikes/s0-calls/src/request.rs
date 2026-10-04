//! The typed message contract shared by server and Rust client
//! (SERVICES.md §4.1): a request names its opcode, its response and its
//! error type once, so the two sides cannot disagree. Bytes exist only at
//! the wire; everything a game writes is typed.

use std::collections::HashMap;
use std::fmt;
use std::marker::PhantomData;

use prost::Message;
use tokio::sync::mpsc;

use crate::wire::{Frame, RpcResponse};

/// A correlated request: one opcode, one response type, one error type.
/// prost by default (see SERVICES.md §4.1 for swapping the format).
pub trait Request: Message + Default + Send + 'static {
    const OP: u16;
    type Response: Message + Default + Send + 'static;
    type Error: Message + Default + Send + 'static;
}

/// A pushed event: one opcode, one payload type, no answer.
pub trait Event: Message + Default + Send + 'static {
    const OP: u16;
}

/// A transport session key (the engine's `ConnectionId`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ConnId(pub u64);

/// Which path a client request's answer takes back.
#[derive(Debug, Clone)]
pub enum Via {
    /// The room's per-tick private frame (today's path).
    Room,
    /// The connection's own outbound path (a hub request: the connection
    /// is not a member here, so there is no private frame to ride).
    Hub(mpsc::Sender<Frame>),
}

/// A client request the logic has accepted and still owes an answer to.
/// Not `Clone`: it is answered at most once (`TickCtx::reply` consumes
/// it). Dropping it unanswered is allowed — the engine's timeout sweep
/// answers it. Carry it as (part of) a call's tag to answer later.
pub struct ClientCall<R> {
    pub(crate) conn: ConnId,
    pub(crate) id: u64,
    pub(crate) via: Via,
    _r: PhantomData<fn() -> R>,
}

impl<R> ClientCall<R> {
    pub(crate) fn new(conn: ConnId, id: u64, via: Via) -> Self {
        Self {
            conn,
            id,
            via,
            _r: PhantomData,
        }
    }

    pub fn conn(&self) -> ConnId {
        self.conn
    }

    pub fn id(&self) -> u64 {
        self.id
    }
}

impl<R> fmt::Debug for ClientCall<R> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ClientCall")
            .field("conn", &self.conn)
            .field("id", &self.id)
            .finish_non_exhaustive()
    }
}

/// The typed answer's wire shape (`ok = false` + non-empty payload = the
/// typed game error).
pub(crate) fn encode_reply<R: Request>(
    id: u64,
    result: Result<R::Response, R::Error>,
) -> RpcResponse {
    match result {
        Ok(resp) => RpcResponse {
            id,
            ok: true,
            op: u32::from(R::OP),
            reason: String::new(),
            payload: resp.encode_to_vec(),
        },
        Err(err) => RpcResponse {
            id,
            ok: false,
            op: u32::from(R::OP),
            reason: String::new(),
            payload: err.encode_to_vec(),
        },
    }
}

/// A second registration of the same opcode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DuplicateOp(pub u16);

/// Why the engine answered a request itself (the game never sees it).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rejection {
    NoHandler,
    Undecodable,
}

type Decoder<In> = Box<dyn Fn(ConnId, u64, Via, &[u8]) -> Result<In, Rejection> + Send>;

/// Opcode → typed decoder into the game's request enum `In`, built once
/// when the room is built. Duplicate opcodes are refused at registration;
/// unknown opcodes and undecodable payloads are answered and counted by
/// the engine.
pub struct RequestTable<In> {
    by_op: HashMap<u16, Decoder<In>>,
}

impl<In> Default for RequestTable<In> {
    fn default() -> Self {
        Self {
            by_op: HashMap::new(),
        }
    }
}

impl<In: 'static> RequestTable<In> {
    /// Register `R`: a decoded request becomes `wrap(call, req)` — usually
    /// a tuple variant of the game's enum (`ShopReq::Buy`).
    pub fn add<R: Request>(&mut self, wrap: fn(ClientCall<R>, R) -> In) -> Result<(), DuplicateOp> {
        if self.by_op.contains_key(&R::OP) {
            return Err(DuplicateOp(R::OP));
        }
        self.by_op.insert(
            R::OP,
            Box::new(move |conn, id, via, bytes| {
                let req = R::decode(bytes).map_err(|_| Rejection::Undecodable)?;
                Ok(wrap(ClientCall::new(conn, id, via), req))
            }),
        );
        Ok(())
    }

    pub(crate) fn decode(
        &self,
        conn: ConnId,
        id: u64,
        op: u16,
        via: Via,
        payload: &[u8],
    ) -> Result<In, Rejection> {
        match self.by_op.get(&op) {
            Some(decode) => decode(conn, id, via, payload),
            None => Err(Rejection::NoHandler),
        }
    }
}
