//! The room's input log (feature `record`, `docs/SERVICES.md` §10.1):
//! per tick, what entered the room's LOGIC, in the order the room
//! processed it — the CONTROL hooks (join, leave, detach, resume, a hold's
//! end, a batch dropped or resumed, a session's end), the RESPONSES (every
//! call that ended, with its type and a copy of its outcome; the late
//! ones by key), the READ (the requests the table took, the actions and
//! the requests `ingest` and `handle_request` saw) and the phase markers
//! (SYSTEMS, the tick's end), the EVENTS the table admitted
//! (`docs/EVENTS.md` §11: with their owner resolved, like a READ) — plus
//! `dt` and the tick's state hash.
//!
//! Recorded at the logic boundary, not the transport: a refused,
//! congested or malformed input never reached the logic and is not in
//! the log; a replay ([`Replay`](crate::determinism::Replay)) feeds the
//! same logic the same inputs and never calls a service.

use std::borrow::Cow;
use std::collections::HashMap;
use std::time::Duration;

use bytes::Bytes;

use crate::calls::{
    Call, CallError, CallKey, Erased, Identity, Outcome, Owner, Refusal, WireMessage,
};
use crate::channel::{Inbox, Mailbox};
use crate::events::PublisherId;
use crate::id::{ConnectionId, EntityId, PlayerId, RoomId};
use crate::room::{DisconnectCause, ExpireTo, RoomConfig};

/// The log format's version (the file's header carries it; a reader
/// refuses another).
///
/// 2: a join carries its admission, a hold's release ask its answer
/// (B175).
pub const FORMAT_VERSION: u16 = 2;

/// What a log was recorded with: the formats and versions (a replay
/// needs the same binary — `replay_diverged` catches a different one,
/// it cannot fix it), the room, its seed, and the knobs the replayed
/// table needs.
#[derive(Debug, Clone, PartialEq)]
pub struct LogHeader {
    /// [`FORMAT_VERSION`] when written.
    pub format: u16,
    /// The engine's version (`gsb-core`'s crate version).
    pub engine: String,
    /// The game's own version string ([`Recorder::game`]).
    pub game: String,
    /// The room.
    pub room: u64,
    /// The room's seed (its generator and call-key origin).
    pub seed: u64,
    /// The room's rate.
    pub tick_hz: f64,
    /// The table's share per service.
    pub call_share: u32,
    /// The External caps (the request path's decisions depend on them).
    pub max_pending_requests: usize,
    /// The per-connection External cap.
    pub max_pending_requests_per_conn: usize,
    /// The External timeout.
    pub request_timeout: Duration,
}

impl LogHeader {
    pub(crate) fn of(config: &RoomConfig, seed: u64, game: &str) -> Self {
        Self {
            format: FORMAT_VERSION,
            engine: env!("CARGO_PKG_VERSION").to_owned(),
            game: game.to_owned(),
            room: config.id.0,
            seed,
            tick_hz: config.tick_hz,
            call_share: config.call_share,
            max_pending_requests: config.max_pending_requests,
            max_pending_requests_per_conn: config.max_pending_requests_per_conn,
            request_timeout: config.request_timeout,
        }
    }

    /// The configuration a replay of this log runs with.
    pub fn config(&self) -> RoomConfig {
        RoomConfig {
            id: RoomId(self.room),
            tick_hz: self.tick_hz,
            call_share: self.call_share,
            max_pending_requests: self.max_pending_requests,
            max_pending_requests_per_conn: self.max_pending_requests_per_conn,
            request_timeout: self.request_timeout,
            seed: Some(self.seed),
            // The replay feeds every ending from the log: no sweep.
            call_timeout: None,
            ..RoomConfig::default()
        }
    }
}

/// How a recorded ending reached the table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Arrival {
    /// A call ended (an answer, a refusal, the table's own deadline …).
    Ended,
    /// An External request's deadline passed (the built-in path).
    Deadline,
    /// An answer for a key the table no longer held (counted late).
    Late,
}

/// A recorded outcome: the value itself (in memory), its encoding (read
/// from a file, decoded by the replaying table's [`Codecs`]), or nothing
/// (a late answer; or a type no codec covered when it was written).
pub enum RecordedOutcome {
    /// The outcome (`Outcome<C>`, erased).
    Value(Erased),
    /// The outcome's encoding.
    Encoded(Bytes),
    /// None.
    Missing,
}

impl std::fmt::Debug for RecordedOutcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Value(_) => f.write_str("Value(..)"),
            Self::Encoded(b) => write!(f, "Encoded({} bytes)", b.len()),
            Self::Missing => f.write_str("Missing"),
        }
    }
}

/// One ending the RESPONSES phase took in.
#[derive(Debug)]
pub struct Recorded {
    /// The call's key.
    pub key: CallKey,
    /// The call's type (its `type_name`): a replay applies an outcome only
    /// to a pending call of the same type (`replay_diverged` otherwise —
    /// the spike's finding: a shifted log made a key name another call).
    pub call: Cow<'static, str>,
    /// How it arrived.
    pub arrival: Arrival,
    /// Whether the call's owner was alive (its result handed out) — the
    /// replay answers the owner test with it.
    pub alive: bool,
    /// The outcome.
    pub outcome: RecordedOutcome,
}

/// One action `ingest` saw.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordedAction {
    /// The session.
    pub conn: ConnectionId,
    /// The room-resolved player.
    pub player: PlayerId,
    /// The action's op.
    pub op: u16,
    /// Its payload.
    pub payload: Bytes,
}

/// One WORLD leg the event table admitted at EVENTS (`docs/EVENTS.md`
/// §11): what the game could take, its owner resolved. A replay puts it
/// into the same table — no router, no publisher, no service.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordedEvent {
    /// The addressee: a player by identity, `None` for the whole room.
    pub to: Option<Identity>,
    /// The owner the addressee was bound to ([`Owner::ACTOR`] room-wide).
    pub owner: Owner,
    /// The publisher.
    pub publisher: PublisherId,
    /// The WORLD leg's sequence number.
    pub seq: u64,
    /// The event's op.
    pub op: u16,
    /// Its encoded body.
    pub payload: Bytes,
}

/// One input to the room's logic, in the order the room processed it.
#[derive(Debug)]
#[non_exhaustive]
pub enum Input {
    /// `on_join_verified(conn, identity, claims)` and the admission it
    /// answered (a replay compares its own: B175).
    Join {
        /// The session.
        conn: ConnectionId,
        /// The resume key.
        identity: String,
        /// The verified claims.
        claims: Option<Bytes>,
        /// The admitted player.
        player: PlayerId,
        /// Its entity.
        entity: EntityId,
    },
    /// `on_resume(identity, conn, player, entity)`.
    Resume {
        /// The resume key.
        identity: String,
        /// The new session.
        conn: ConnectionId,
        /// The parked player.
        player: PlayerId,
        /// Its entity.
        entity: EntityId,
    },
    /// `on_leave(player)`.
    Leave {
        /// The player.
        player: PlayerId,
    },
    /// `on_disconnect_with(player, identity, cause)`.
    Disconnect {
        /// The player.
        player: PlayerId,
        /// Its resume key.
        identity: String,
        /// Why.
        cause: DisconnectCause,
    },
    /// `may_release(player)` and its answer (a replay compares its own:
    /// B175).
    Release {
        /// The parked player.
        player: PlayerId,
        /// Whether the logic released it.
        released: bool,
    },
    /// `on_detach_expired(player, to)`.
    Expire {
        /// The parked player.
        player: PlayerId,
        /// Where it goes.
        to: ExpireTo,
    },
    /// `on_batch_dropped(player, snapshot)`.
    BatchDropped {
        /// The player.
        player: PlayerId,
        /// Whether the batch carried a snapshot.
        snapshot: bool,
    },
    /// `on_batch_resumed(player)`.
    BatchResumed {
        /// The player.
        player: PlayerId,
    },
    /// A session ended: the table abandons what it owed it.
    SessionEnd {
        /// The session.
        conn: ConnectionId,
    },
    /// A session parked (detach-hold): the table abandons what it owed
    /// it but keeps its explicit bind (BACKLOG B156).
    SessionPark {
        /// The session.
        conn: ConnectionId,
    },
    /// A parked session resumed on a new transport: its explicit bind
    /// moves (B156).
    SessionMove {
        /// The parked session.
        from: ConnectionId,
        /// The resumed one.
        to: ConnectionId,
    },
    /// RESPONSES: the endings the table took in, in order.
    Responses(Vec<Recorded>),
    /// READ: one request of a claimed op, as the table took it.
    Read {
        /// The session.
        conn: ConnectionId,
        /// Its player.
        player: PlayerId,
        /// The sender's verified identity.
        from: Identity,
        /// The session's default owner (`call_owner`).
        owner: Option<Owner>,
        /// The correlation id.
        wire_id: u64,
        /// The op.
        op: u16,
        /// The payload.
        payload: Bytes,
    },
    /// EVENTS: the WORLD legs the event table admitted this tick, in
    /// drain order (only when there were any; file tag 18).
    Events(Vec<RecordedEvent>),
    /// CONVERT: `ingest` with these actions.
    Actions(Vec<RecordedAction>),
    /// One request `handle_request` saw (past the congestion gate).
    Request {
        /// The session.
        conn: ConnectionId,
        /// Its player.
        player: PlayerId,
        /// The correlation id.
        id: u64,
        /// The op.
        op: u16,
        /// The payload.
        payload: Bytes,
    },
    /// SYSTEMS (`update_with_calls`) and FLUSH.
    Systems,
    /// The tick's end (the table closes what nobody took).
    EndTick,
}

/// One tick of the log.
#[derive(Debug)]
pub struct TickRecord {
    /// The global tick the room stepped on.
    pub tick: u64,
    /// The step's `dt`.
    pub dt: Duration,
    /// What entered the logic, in order.
    pub inputs: Vec<Input>,
    /// The step's state hash (`None`: the game hashes nothing).
    pub hash: Option<u64>,
}

/// What a recording room sends its sink.
#[derive(Debug)]
pub enum RecordMsg {
    /// First: the header, and the codecs a file writer encodes outcomes
    /// with.
    Start(LogHeader, Codecs),
    /// Then one per step.
    Tick(TickRecord),
}

/// A room's recording sink: a bounded mailbox (the room `try_send`s once
/// per step; a full or gone sink loses the record, counted
/// `records_dropped`) and the game's version string.
#[derive(Debug, Clone)]
pub struct Recorder {
    /// The sink.
    pub sink: Mailbox<RecordMsg>,
    /// The game's version (in the header).
    pub game: String,
}

impl Recorder {
    /// A recorder and the inbox its records arrive in (`capacity`
    /// messages; drain it with [`InputLog::collect`] or
    /// [`spawn_writer`](crate::determinism::spawn_writer)).
    pub fn channel(capacity: usize, game: &str) -> (Self, Inbox<RecordMsg>) {
        let (sink, inbox) = crate::channel::channel(capacity.max(1));
        (
            Self {
                sink,
                game: game.to_owned(),
            },
            inbox,
        )
    }
}

/// A whole log, in memory.
#[derive(Debug)]
pub struct InputLog {
    /// The header.
    pub header: LogHeader,
    /// The ticks, in order.
    pub ticks: Vec<TickRecord>,
}

impl InputLog {
    /// Take what `inbox` holds into a log (`None` if its header never
    /// arrived — the first record was lost). Ticks append in order.
    pub fn collect(inbox: &mut Inbox<RecordMsg>) -> Option<Self> {
        let mut log: Option<Self> = None;
        while let Ok(msg) = inbox.try_recv() {
            match msg {
                RecordMsg::Start(header, _) => {
                    log = Some(Self {
                        header,
                        ticks: Vec::new(),
                    })
                }
                RecordMsg::Tick(t) => {
                    if let Some(l) = &mut log {
                        l.ticks.push(t);
                    }
                }
            }
        }
        log
    }

    /// Append what `inbox` holds now (a log collected while recording).
    pub fn extend_from(&mut self, inbox: &mut Inbox<RecordMsg>) {
        while let Ok(msg) = inbox.try_recv() {
            if let RecordMsg::Tick(t) = msg {
                self.ticks.push(t);
            }
        }
    }

    /// The recorded hashes, in tick order.
    pub fn hashes(&self) -> Vec<Option<u64>> {
        self.ticks.iter().map(|t| t.hash).collect()
    }
}

/// One call type's outcome codec.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Codec {
    pub(crate) encode: fn(&Erased) -> Option<Vec<u8>>,
    pub(crate) decode: fn(&[u8]) -> Option<Erased>,
}

/// How a call type's outcomes are written to (and read from) a file,
/// by type name. A table registers one for every `forward` rule's call
/// (its answer and error are a request's, which have a wire form) and for
/// what the game names with [`Calls::record_as`](crate::calls::Calls::record_as);
/// an outcome of a type without one is written as missing (counted by
/// the writer, `outcomes_unencodable`) and a replay of the file counts
/// it `replay_undecodable`. A log replayed from memory needs none.
#[derive(Debug, Clone, Default)]
pub struct Codecs {
    map: HashMap<&'static str, Codec>,
}

impl Codecs {
    /// Cover `C`'s outcomes.
    pub fn register<C>(&mut self)
    where
        C: Call<Response: WireMessage, Error: WireMessage>,
    {
        self.map.insert(
            std::any::type_name::<C>(),
            Codec {
                encode: encode_outcome::<C>,
                decode: decode_outcome::<C>,
            },
        );
    }

    pub(crate) fn insert(&mut self, name: &'static str, codec: Codec) {
        self.map.insert(name, codec);
    }

    /// Whether `call` (a type name) is covered.
    pub fn covers(&self, call: &str) -> bool {
        self.map.contains_key(call)
    }

    pub(crate) fn encode(&self, call: &str, o: &Erased) -> Option<Vec<u8>> {
        self.map.get(call).and_then(|c| (c.encode)(o))
    }

    pub(crate) fn decode(&self, call: &str, b: &[u8]) -> Option<Erased> {
        self.map.get(call).and_then(|c| (c.decode)(b))
    }
}

// Outcome encoding: one tag byte, then the body.
const OK: u8 = 0;
const SERVICE: u8 = 1;
const REFUSED: u8 = 2;
const TIMED_OUT: u8 = 3;
const ABANDONED: u8 = 4;

pub(crate) fn refusal_code(r: Refusal) -> u8 {
    match r {
        Refusal::Full => 0,
        Refusal::Gone => 1,
        Refusal::Share => 2,
        Refusal::Shed => 3,
    }
}

pub(crate) fn refusal_of(b: u8) -> Option<Refusal> {
    Some(match b {
        0 => Refusal::Full,
        1 => Refusal::Gone,
        2 => Refusal::Share,
        3 => Refusal::Shed,
        _ => return None,
    })
}

/// Encode an outcome whose answer and error encode with `ok`/`err`.
pub(crate) fn encode_with<C: Call>(
    o: &Erased,
    ok: fn(&C::Response) -> Vec<u8>,
    err: fn(&C::Error) -> Vec<u8>,
) -> Option<Vec<u8>> {
    let o = o.downcast_ref::<Outcome<C>>()?;
    let mut out = Vec::new();
    match o {
        Ok(r) => {
            out.push(OK);
            out.extend(ok(r));
        }
        Err(CallError::Service(e)) => {
            out.push(SERVICE);
            out.extend(err(e));
        }
        Err(CallError::Refused(r)) => out.extend([REFUSED, refusal_code(*r)]),
        Err(CallError::TimedOut) => out.push(TIMED_OUT),
        Err(CallError::Abandoned) => out.push(ABANDONED),
    }
    Some(out)
}

/// Decode an outcome whose answer and error decode with `ok`/`err`.
pub(crate) fn decode_with<C: Call>(
    b: &[u8],
    ok: fn(&[u8]) -> Option<C::Response>,
    err: fn(&[u8]) -> Option<C::Error>,
) -> Option<Erased> {
    let (&tag, body) = b.split_first()?;
    let o: Outcome<C> = match tag {
        OK => Ok(ok(body)?),
        SERVICE => Err(CallError::Service(err(body)?)),
        REFUSED => Err(CallError::Refused(refusal_of(*body.first()?)?)),
        TIMED_OUT => Err(CallError::TimedOut),
        ABANDONED => Err(CallError::Abandoned),
        _ => return None,
    };
    Some(Box::new(o))
}

fn encode_outcome<C>(o: &Erased) -> Option<Vec<u8>>
where
    C: Call<Response: WireMessage, Error: WireMessage>,
{
    encode_with::<C>(o, |r| r.to_wire(), |e| e.to_wire())
}

fn decode_outcome<C>(b: &[u8]) -> Option<Erased>
where
    C: Call<Response: WireMessage, Error: WireMessage>,
{
    decode_with::<C>(
        b,
        |b| C::Response::from_wire(b).ok(),
        |b| C::Error::from_wire(b).ok(),
    )
}
