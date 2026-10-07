//! Typed requests (`docs/SERVICES.md` §14, round q3): send a
//! [`Request`] — a `.proto`-declared rpc, its impls written by
//! `gsb-build` — and get back its typed answer, its TYPED game error, an
//! engine error, a local timeout or the session's end. Never a hang.
//!
//! Two layers:
//!
//! - [`Requests`] — the sans-IO core (the FFI core's request half): it
//!   numbers requests, builds their `RPC_REQ` frames, matches the
//!   answers it is fed, times them out on the clock it is given, closes
//!   all of them when the session ends ([`Requests::end_session`]; S16),
//!   and forgets one on demand ([`Requests::forget`]). It does no I/O
//!   and needs no runtime.
//! - [`RequestClient`] — that core over a [`Conn`] (any door): one
//!   `client.request::<R>(&body).await`, or [`RequestClient::send`] then
//!   [`RequestClient::wait`] to keep several in flight. Its wait is ONE
//!   bounded await per turn (`Conn::recv` until the next deadline) — no
//!   multiplexing; the frames that are not answers wait in a bounded
//!   queue for the caller ([`RequestClient::take_frames`]).
//!
//! Where answers ride is the GAME's choice (the demo and the kit put
//! `gsb.base.RpcResponse` in their private frame's `responses`), so the
//! client takes an extractor: `FnMut(&FrameBody) -> Option<Vec<RpcResponse>>`
//! (`None`: not an answer-carrying frame).
//!
//! Every loss is counted ([`RequestCounters`]): a request timed out or
//! cut by the session's end, an answer that came after its request was
//! forgotten or timed out, an answer whose payload did not decode as the
//! declared type, a non-answer frame the bounded queue had to drop.

use std::collections::{HashMap, VecDeque};
use std::marker::PhantomData;
use std::time::{Duration, Instant};

use gsb_protocol::FrameBody;
use gsb_protocol::base::{RpcRequest, RpcResponse};
use gsb_protocol::request::{Request, WireMessage};
use prost::Message;

use crate::conn::{Conn, Recv};

/// How a typed request ended, when not with its answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RequestError<E> {
    /// The game answered with the request's typed error
    /// (`RpcResponse.game_error`; a default-valued error is zero bytes
    /// and still arrives here).
    Game(E),
    /// The engine answered instead of the game (no handler, refused,
    /// timed out on the server, a legacy `Reject` …): its reason.
    Engine(String),
    /// No answer within the client's own timeout.
    TimedOut,
    /// The session ended before the answer came (S16): no answer will.
    SessionEnded,
    /// The answer's payload did not decode as the declared type (a
    /// client and server built from different `.proto`s).
    Undecodable,
    /// The caller forgot the request ([`Requests::forget`]) and then
    /// waited for it anyway.
    Forgotten,
}

impl<E: std::fmt::Debug> std::fmt::Display for RequestError<E> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RequestError::Game(e) => write!(f, "game error: {e:?}"),
            RequestError::Engine(r) => write!(f, "engine answered: {r}"),
            RequestError::TimedOut => f.write_str("request timed out (client)"),
            RequestError::SessionEnded => f.write_str("session ended before the answer"),
            RequestError::Undecodable => f.write_str("answer did not decode as its type"),
            RequestError::Forgotten => f.write_str("request was forgotten"),
        }
    }
}

impl<E: std::fmt::Debug> std::error::Error for RequestError<E> {}

/// What a typed request ends with.
pub type Answer<R> = Result<
    <R as gsb_protocol::request::Call>::Response,
    RequestError<<R as gsb_protocol::request::Call>::Error>,
>;

/// One request in flight: its id, typed by the request it was for.
#[derive(Debug)]
pub struct Ticket<R> {
    id: u64,
    _r: PhantomData<fn() -> R>,
}

impl<R> Ticket<R> {
    /// The correlation id (what [`Requests::forget`] takes).
    pub fn id(&self) -> u64 {
        self.id
    }
}

/// The client side's counts — one meaning each.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RequestCounters {
    /// Requests started.
    pub sent: u64,
    /// Answered `ok`.
    pub ok: u64,
    /// Answered with the typed game error.
    pub game_errors: u64,
    /// Answered by the engine (a reason).
    pub engine_errors: u64,
    /// Timed out on the client's clock.
    pub timed_out: u64,
    /// Cut by the session's end.
    pub session_ended: u64,
    /// Forgotten by the caller while in flight.
    pub forgotten: u64,
    /// An answer for a forgotten request (dropped).
    pub answers_forgotten: u64,
    /// An answer matching no request in flight — it came after the
    /// client's timeout, or it is a duplicate (dropped).
    pub answers_late: u64,
    /// An answer (ok or game error) whose payload did not decode as the
    /// request's declared type.
    pub undecodable: u64,
}

enum Settled {
    Answer(RpcResponse),
    TimedOut,
    SessionEnded,
}

struct Pending {
    due: Instant,
    forgotten: bool,
}

/// The sans-IO request table (see the module docs).
pub struct Requests {
    timeout: Duration,
    next_id: u64,
    pending: HashMap<u64, Pending>,
    ended: HashMap<u64, Settled>,
    counts: RequestCounters,
}

impl Requests {
    /// A table whose requests time out `timeout` after they start (pick
    /// it above the server's own request timeout, so the server's answer
    /// — even its timeout answer — normally wins).
    pub fn new(timeout: Duration) -> Self {
        Self {
            timeout,
            next_id: 0,
            pending: HashMap::new(),
            ended: HashMap::new(),
            counts: RequestCounters::default(),
        }
    }

    /// Start request `body` at `now`: its ticket and the `RPC_REQ` frame
    /// to send. Ids start at 1 (0 cannot correlate).
    pub fn start<R: Request>(&mut self, body: &R, now: Instant) -> (Ticket<R>, FrameBody) {
        self.start_within(body, now, self.timeout)
    }

    /// [`Self::start`] with this request's own `timeout`.
    pub fn start_within<R: Request>(
        &mut self,
        body: &R,
        now: Instant,
        timeout: Duration,
    ) -> (Ticket<R>, FrameBody) {
        self.next_id += 1;
        let id = self.next_id;
        self.pending.insert(
            id,
            Pending {
                due: now + timeout,
                forgotten: false,
            },
        );
        self.counts.sent += 1;
        let env = RpcRequest {
            id,
            op: u32::from(R::OP),
            payload: body.to_wire(),
        };
        let frame = FrameBody::new(gsb_protocol::op::base::RPC_REQ, env.encode_to_vec());
        (
            Ticket {
                id,
                _r: PhantomData,
            },
            frame,
        )
    }

    /// Feed one answer from the wire.
    pub fn answer(&mut self, r: RpcResponse) {
        match self.pending.remove(&r.id) {
            None => self.counts.answers_late += 1,
            Some(p) if p.forgotten => self.counts.answers_forgotten += 1,
            Some(_) => {
                if r.ok {
                    self.counts.ok += 1;
                } else if r.game_error {
                    self.counts.game_errors += 1;
                } else {
                    self.counts.engine_errors += 1;
                }
                self.ended.insert(r.id, Settled::Answer(r));
            }
        }
    }

    /// Time out what is due at `now` (a forgotten request just leaves).
    pub fn sweep(&mut self, now: Instant) {
        let due: Vec<u64> = self
            .pending
            .iter()
            .filter(|(_, p)| p.due <= now)
            .map(|(id, _)| *id)
            .collect();
        for id in due {
            if let Some(p) = self.pending.remove(&id)
                && !p.forgotten
            {
                self.counts.timed_out += 1;
                self.ended.insert(id, Settled::TimedOut);
            }
        }
    }

    /// The session ended: every request in flight ends with
    /// [`RequestError::SessionEnded`] (a forgotten one just leaves).
    pub fn end_session(&mut self) {
        for (id, p) in self.pending.drain() {
            if !p.forgotten {
                self.counts.session_ended += 1;
                self.ended.insert(id, Settled::SessionEnded);
            }
        }
    }

    /// The caller gave request `id` up: whatever comes for it is
    /// dropped (and counted, `answers_forgotten`); an outcome already in
    /// hand is discarded. `false`: no such request.
    pub fn forget(&mut self, id: u64) -> bool {
        let had_end = self.ended.remove(&id).is_some();
        let pending = match self.pending.get_mut(&id) {
            Some(p) if !p.forgotten => {
                p.forgotten = true;
                true
            }
            _ => false,
        };
        if pending || had_end {
            self.counts.forgotten += 1;
        }
        pending || had_end
    }

    /// The outcome of `t`, once it has one (`None`: still in flight, or
    /// forgotten).
    pub fn take<R: Request>(&mut self, t: &Ticket<R>) -> Option<Answer<R>> {
        let ended = self.ended.remove(&t.id)?;
        Some(match ended {
            Settled::TimedOut => Err(RequestError::TimedOut),
            Settled::SessionEnded => Err(RequestError::SessionEnded),
            Settled::Answer(r) if r.ok => R::Response::from_wire(&r.payload).map_err(|_| {
                self.counts.undecodable += 1;
                RequestError::Undecodable
            }),
            Settled::Answer(r) if r.game_error => match R::Error::from_wire(&r.payload) {
                Ok(e) => Err(RequestError::Game(e)),
                Err(_) => {
                    self.counts.undecodable += 1;
                    Err(RequestError::Undecodable)
                }
            },
            Settled::Answer(r) => Err(RequestError::Engine(r.reason)),
        })
    }

    /// Is `id` still owed an outcome (in flight and not forgotten, or
    /// ended and not yet taken)?
    pub fn is_open(&self, id: u64) -> bool {
        self.ended.contains_key(&id) || self.pending.get(&id).is_some_and(|p| !p.forgotten)
    }

    /// The earliest deadline in flight (what a waiting loop sleeps to).
    pub fn next_due(&self) -> Option<Instant> {
        self.pending.values().map(|p| p.due).min()
    }

    /// Requests in flight (forgotten ones included until they leave).
    pub fn in_flight(&self) -> usize {
        self.pending.len()
    }

    /// The counts so far.
    pub fn counters(&self) -> RequestCounters {
        self.counts
    }
}

/// Picks the answers out of a frame (`None`: not an answer-carrying
/// frame). The game's choice of where answers ride.
pub type Extract = Box<dyn FnMut(&FrameBody) -> Option<Vec<RpcResponse>> + Send>;

/// The longest single wait on a quiet connection, so a deadline is
/// never overslept by much even when none is near.
const MAX_WINDOW: Duration = Duration::from_millis(250);

/// [`Requests`] over a [`Conn`] (see the module docs).
pub struct RequestClient {
    conn: Conn,
    requests: Requests,
    extract: Extract,
    frames: VecDeque<FrameBody>,
    frames_cap: usize,
    frames_dropped: u64,
    ended: bool,
}

impl RequestClient {
    /// Typed requests over `conn` (already authenticated and joined),
    /// each timing out `timeout` after it is sent; `extract` finds the
    /// answers; up to `frames_cap` other frames are kept for the caller
    /// (past it the OLDEST is dropped and counted — the newest state
    /// wins).
    pub fn new(conn: Conn, timeout: Duration, extract: Extract, frames_cap: usize) -> Self {
        Self {
            conn,
            requests: Requests::new(timeout),
            extract,
            frames: VecDeque::new(),
            frames_cap,
            frames_dropped: 0,
            ended: false,
        }
    }

    /// Send `body` and wait for its outcome.
    pub async fn request<R: Request>(&mut self, body: &R) -> Answer<R> {
        let t = self.send(body).await;
        self.wait(t).await
    }

    /// Send `body`; its ticket. A send that fails ends the session (the
    /// ticket then waits to [`RequestError::SessionEnded`]).
    pub async fn send<R: Request>(&mut self, body: &R) -> Ticket<R> {
        let timeout = self.requests.timeout;
        self.send_within(body, timeout).await
    }

    /// [`Self::send`] with this request's own client-side `timeout`.
    pub async fn send_within<R: Request>(&mut self, body: &R, timeout: Duration) -> Ticket<R> {
        let (t, frame) = self.requests.start_within(body, Instant::now(), timeout);
        if self.ended {
            self.requests.end_session();
        } else if self.conn.send(frame.op, &frame.payload).await.is_err() {
            self.end();
        }
        t
    }

    /// Wait for `t`'s outcome, reading the connection meanwhile (one
    /// bounded `recv` per turn, up to the next deadline).
    pub async fn wait<R: Request>(&mut self, t: Ticket<R>) -> Answer<R> {
        loop {
            if let Some(out) = self.requests.take(&t) {
                return out;
            }
            if !self.requests.is_open(t.id) {
                return Err(RequestError::Forgotten);
            }
            let now = Instant::now();
            self.requests.sweep(now);
            if let Some(out) = self.requests.take(&t) {
                return out;
            }
            if self.ended {
                // Nothing left to read; anything still pending ends now.
                self.requests.end_session();
                continue;
            }
            let window = self
                .requests
                .next_due()
                .map_or(MAX_WINDOW, |d| d.saturating_duration_since(now))
                .min(MAX_WINDOW);
            match self.conn.recv(window).await {
                Ok(Recv::Frame(f)) => self.feed(f),
                Ok(Recv::Quiet) => {}
                Ok(Recv::Closed) | Err(_) => self.end(),
            }
        }
    }

    /// Give `id` up (see [`Requests::forget`]).
    pub fn forget(&mut self, id: u64) -> bool {
        self.requests.forget(id)
    }

    /// The frames read while waiting that carried no answer, oldest first.
    pub fn take_frames(&mut self) -> impl Iterator<Item = FrameBody> + '_ {
        self.frames.drain(..)
    }

    /// Non-answer frames dropped from the full queue.
    pub fn frames_dropped(&self) -> u64 {
        self.frames_dropped
    }

    /// The request counts.
    pub fn counters(&self) -> RequestCounters {
        self.requests.counters()
    }

    /// Requests in flight.
    pub fn in_flight(&self) -> usize {
        self.requests.in_flight()
    }

    /// The connection (to send actions, a leave …).
    pub fn conn_mut(&mut self) -> &mut Conn {
        &mut self.conn
    }

    /// Has the session ended (its end read, or a send failed)?
    pub fn is_ended(&self) -> bool {
        self.ended
    }

    fn feed(&mut self, f: FrameBody) {
        match (self.extract)(&f) {
            Some(answers) => {
                for a in answers {
                    self.requests.answer(a);
                }
            }
            None if self.frames_cap == 0 => self.frames_dropped += 1,
            None => {
                if self.frames.len() >= self.frames_cap {
                    self.frames.pop_front();
                    self.frames_dropped += 1;
                }
                self.frames.push_back(f);
            }
        }
    }

    fn end(&mut self) {
        self.ended = true;
        self.requests.end_session();
    }
}

#[cfg(test)]
mod tests;
