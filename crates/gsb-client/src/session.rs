//! The session steps of the base protocol, as plain async functions over
//! a [`Conn`]: AUTH (with or without a ticket), JOIN, HEARTBEAT, LEAVE —
//! and the frames each one sends, for a caller that drives its own loop.
//!
//! A step sends its request and waits (bounded) for its reply. Frames
//! that arrive meanwhile and are not the reply — game frames racing
//! ahead of a join result, a snapshot during a heartbeat — go to the
//! caller's `other` sink, in order; nothing is dropped behind its back.
//! An `ERROR` frame ends the wait as [`ClientError::Server`], typed.
//!
//! Policy stays with the caller: whether to retry a refused join, when
//! to reconnect, how long to back off. The resume path is the protocol's
//! own: a new connection that authenticates with the SAME
//! [`Credentials`] (the resume key) and joins the same room gets its
//! parked entity back — the join result's entity equals the old one.

use std::time::{Duration, Instant};

use gsb_protocol::FrameBody;
use gsb_protocol::base::{Auth, AuthResult, Heartbeat, HeartbeatAck, JoinRoom, JoinRoomResult};
use gsb_protocol::op::base as op;
use prost::Message;

use crate::conn::{Conn, Recv};
use crate::error::{ClientError, ServerError};

/// Who this client is: the resume key. Local auth takes `name` as the
/// identity; a ticket-auth server takes the identity from `ticket` (the
/// name is then only a claim). Keep it to resume: re-presenting the same
/// credentials on a new connection is what makes a join a resume.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Credentials {
    pub name: String,
    pub ticket: Vec<u8>,
}

impl Credentials {
    /// Local-auth credentials: the name is the identity.
    pub fn named(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            ticket: Vec::new(),
        }
    }

    /// The same credentials presenting `ticket`.
    pub fn with_ticket(mut self, ticket: impl Into<Vec<u8>>) -> Self {
        self.ticket = ticket.into();
        self
    }
}

/// The AUTH frame (it states [`gsb_protocol::PROTOCOL_VERSION`]).
pub fn auth_req(c: &Credentials) -> FrameBody {
    let auth = Auth {
        name: c.name.clone(),
        ticket: c.ticket.clone(),
        protocol_version: gsb_protocol::PROTOCOL_VERSION,
    };
    FrameBody::new(op::AUTH_REQ, auth.encode_to_vec())
}

/// The JOIN frame for `room`.
pub fn join_req(room: u64) -> FrameBody {
    FrameBody::new(
        op::JOIN_ROOM_REQ,
        JoinRoom { room_id: room }.encode_to_vec(),
    )
}

/// The LEAVE frame.
pub fn leave_req() -> FrameBody {
    FrameBody::new(op::LEAVE_ROOM_REQ, Vec::new())
}

/// The HEARTBEAT frame carrying `tick`.
pub fn heartbeat(tick: u64) -> FrameBody {
    FrameBody::new(op::HEARTBEAT, Heartbeat { tick }.encode_to_vec())
}

/// A joined session.
#[derive(Debug, Clone)]
pub struct Joined {
    /// The server's answer to AUTH (ticket auth: the identity the
    /// validator extracted, and the room the ticket pins).
    pub auth: AuthResult,
    /// The session's entity (wire id). Equal to a previous session's
    /// entity under the same credentials: the join was a resume.
    pub entity: u64,
}

/// Send AUTH and JOIN pipelined (one write on a stream; the connection
/// actor processes them in order) without waiting for either reply —
/// for a caller whose own loop reads the replies.
pub async fn hello(conn: &mut Conn, c: &Credentials, room: u64) -> std::io::Result<()> {
    conn.send_batch(&[auth_req(c), join_req(room)]).await
}

/// AUTH + JOIN: [`hello`], then the AUTH_RESULT (refused =
/// [`ClientError::AuthRefused`]) and the JOIN_ROOM_RESULT, all within
/// `window`.
pub async fn auth_and_join(
    conn: &mut Conn,
    c: &Credentials,
    room: u64,
    window: Duration,
    other: impl FnMut(FrameBody),
) -> Result<Joined, ClientError> {
    let deadline = Instant::now() + window;
    hello(conn, c, room).await?;
    let mut other = other;
    let auth = auth_reply(conn, deadline, &mut other).await?;
    let f = reply(conn, op::JOIN_ROOM_RESULT, deadline, &mut other).await?;
    let entity = decode::<JoinRoomResult>(&f)?.entity;
    Ok(Joined { auth, entity })
}

/// AUTH alone: send it and wait for the AUTH_RESULT.
pub async fn auth(
    conn: &mut Conn,
    c: &Credentials,
    window: Duration,
    mut other: impl FnMut(FrameBody),
) -> Result<AuthResult, ClientError> {
    let deadline = Instant::now() + window;
    let f = auth_req(c);
    conn.send(f.op, &f.payload).await?;
    auth_reply(conn, deadline, &mut other).await
}

/// JOIN alone (an authenticated connection): the joined entity.
pub async fn join(
    conn: &mut Conn,
    room: u64,
    window: Duration,
    mut other: impl FnMut(FrameBody),
) -> Result<u64, ClientError> {
    let deadline = Instant::now() + window;
    let f = join_req(room);
    conn.send(f.op, &f.payload).await?;
    let r = reply(conn, op::JOIN_ROOM_RESULT, deadline, &mut other).await?;
    Ok(decode::<JoinRoomResult>(&r)?.entity)
}

/// HEARTBEAT: the acknowledged tick.
pub async fn heartbeat_round(
    conn: &mut Conn,
    tick: u64,
    window: Duration,
    mut other: impl FnMut(FrameBody),
) -> Result<u64, ClientError> {
    let deadline = Instant::now() + window;
    let f = heartbeat(tick);
    conn.send(f.op, &f.payload).await?;
    let r = reply(conn, op::HEARTBEAT_ACK, deadline, &mut other).await?;
    Ok(decode::<HeartbeatAck>(&r)?.tick)
}

/// LEAVE: returns once the LEAVE_ROOM_RESULT arrived.
pub async fn leave(
    conn: &mut Conn,
    window: Duration,
    mut other: impl FnMut(FrameBody),
) -> Result<(), ClientError> {
    let deadline = Instant::now() + window;
    let f = leave_req();
    conn.send(f.op, &f.payload).await?;
    reply(conn, op::LEAVE_ROOM_RESULT, deadline, &mut other).await?;
    Ok(())
}

/// Wait until `deadline` for the frame `want`: other frames go to
/// `other`; an `ERROR` frame is [`ClientError::Server`]; EOF is
/// [`ClientError::Closed`]; the deadline is [`ClientError::TimedOut`].
pub async fn reply(
    conn: &mut Conn,
    want: u16,
    deadline: Instant,
    other: &mut impl FnMut(FrameBody),
) -> Result<FrameBody, ClientError> {
    loop {
        let left = deadline
            .checked_duration_since(Instant::now())
            .ok_or(ClientError::TimedOut)?;
        match conn.recv(left).await? {
            Recv::Frame(f) if f.op == want => return Ok(f),
            Recv::Frame(f) if f.op == op::ERROR => {
                return Err(ClientError::Server(ServerError::decode_lossy(&f.payload)));
            }
            Recv::Frame(f) => other(f),
            Recv::Closed => return Err(ClientError::Closed),
            Recv::Quiet => {}
        }
    }
}

async fn auth_reply(
    conn: &mut Conn,
    deadline: Instant,
    other: &mut impl FnMut(FrameBody),
) -> Result<AuthResult, ClientError> {
    let f = reply(conn, op::AUTH_RESULT, deadline, other).await?;
    let auth = decode::<AuthResult>(&f)?;
    if !auth.ok {
        return Err(ClientError::AuthRefused(auth.reason));
    }
    Ok(auth)
}

fn decode<M: Message + Default>(f: &FrameBody) -> Result<M, ClientError> {
    M::decode(&f.payload[..]).map_err(|source| ClientError::Decode { op: f.op, source })
}

#[cfg(test)]
mod tests;
