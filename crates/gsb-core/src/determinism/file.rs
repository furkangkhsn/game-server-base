//! The input log on disk (feature `record`, `docs/SERVICES.md` §19 S5):
//! one file per room — a magic, the format version and the header
//! (engine and game versions, room, seed, knobs), then one
//! length-prefixed frame per tick. Little-endian, hand-written (no new
//! dependency). Outcomes are written with the room table's [`Codecs`]; a
//! type no codec covers is written as missing (counted,
//! `outcomes_unencodable`) and replays as `replay_undecodable`.
//!
//! The writer is a plain thread draining the recorder's inbox with a
//! blocking receive (the room only ever `try_send`s into it): file I/O
//! never touches a tick or a runtime worker.

use std::borrow::Cow;
use std::fs::File;
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use bytes::Bytes;

use crate::calls::{CallKey, Identity, Owner};
use crate::channel::Inbox;
use crate::conn::ServerClose;
use crate::determinism::{
    Arrival, Codecs, FORMAT_VERSION, Input, InputLog, LogHeader, RecordMsg, Recorded,
    RecordedAction, RecordedEvent, RecordedOutcome, TickRecord,
};
use crate::events::PublisherId;
use crate::id::{ConnectionId, PlayerId};
use crate::room::{DisconnectCause, ExpireTo};

const MAGIC: &[u8; 8] = b"GSBINLOG";

/// What a writer did.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct WriterStats {
    /// Tick frames written.
    pub frames_written: u64,
    /// Outcomes written as missing: no codec covered their call type
    /// (the replay of the file cannot apply them).
    pub outcomes_unencodable: u64,
    /// Tick records that arrived before any header (ignored).
    pub frames_headless: u64,
}

/// Why a log file did not read.
#[derive(Debug)]
pub enum ReadError {
    /// The file.
    Io(io::Error),
    /// Not a gsb input log.
    Magic,
    /// A format this reader does not know.
    Version(u16),
    /// A frame or the header did not decode.
    Corrupt(&'static str),
}

impl std::fmt::Display for ReadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "input log: {e}"),
            Self::Magic => f.write_str("input log: not a gsb input log"),
            Self::Version(v) => write!(f, "input log: unknown format version {v}"),
            Self::Corrupt(what) => write!(f, "input log: corrupt {what}"),
        }
    }
}

impl std::error::Error for ReadError {}

impl From<io::Error> for ReadError {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}

/// Write a whole in-memory log to `path` with `codecs`.
pub fn write_log(path: &Path, log: &InputLog, codecs: &Codecs) -> io::Result<WriterStats> {
    let mut w = BufWriter::new(File::create(path)?);
    let mut stats = WriterStats::default();
    w.write_all(&header_bytes(&log.header))?;
    for t in &log.ticks {
        write_frame(&mut w, t, codecs, &mut stats)?;
    }
    w.flush()?;
    Ok(stats)
}

/// Write a recorder's records to `path` as they arrive, on a thread of
/// its own, until every sender is gone (the room stopped, its recorder
/// dropped). The thread's result is the writer's stats.
pub fn spawn_writer(
    path: PathBuf,
    mut inbox: Inbox<RecordMsg>,
) -> std::thread::JoinHandle<io::Result<WriterStats>> {
    std::thread::spawn(move || write_records(&path, &mut inbox))
}

/// [`spawn_writer`]'s body, on the calling thread (the registry's writer
/// threads log its result).
pub(crate) fn write_records(path: &Path, inbox: &mut Inbox<RecordMsg>) -> io::Result<WriterStats> {
    let mut w = BufWriter::new(File::create(path)?);
    let mut stats = WriterStats::default();
    let mut codecs: Option<Codecs> = None;
    while let Some(msg) = inbox.blocking_recv() {
        match msg {
            RecordMsg::Start(header, c) => {
                w.write_all(&header_bytes(&header))?;
                codecs = Some(c);
            }
            RecordMsg::Tick(t) => match &codecs {
                Some(c) => write_frame(&mut w, &t, c, &mut stats)?,
                None => stats.frames_headless += 1,
            },
        }
    }
    w.flush()?;
    Ok(stats)
}

/// Read a log file. Outcomes stay encoded: the replaying table decodes
/// them with its own codecs.
pub fn read_log(path: &Path) -> Result<InputLog, ReadError> {
    let mut r = BufReader::new(File::open(path)?);
    let mut magic = [0u8; 8];
    r.read_exact(&mut magic)?;
    if &magic != MAGIC {
        return Err(ReadError::Magic);
    }
    let mut len = [0u8; 4];
    r.read_exact(&mut len)?;
    let mut buf = vec![0u8; u32::from_le_bytes(len) as usize];
    r.read_exact(&mut buf)?;
    let header = read_header(&mut Dec(&buf))?;
    let mut ticks = Vec::new();
    loop {
        match r.read_exact(&mut len) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => break,
            Err(e) => return Err(e.into()),
        }
        let mut buf = vec![0u8; u32::from_le_bytes(len) as usize];
        r.read_exact(&mut buf)?;
        ticks.push(read_frame(&mut Dec(&buf)).ok_or(ReadError::Corrupt("frame"))?);
    }
    Ok(InputLog { header, ticks })
}

// ---- encoding -----------------------------------------------------------------

struct Enc(Vec<u8>);

impl Enc {
    fn u8(&mut self, v: u8) {
        self.0.push(v);
    }
    fn u16(&mut self, v: u16) {
        self.0.extend(v.to_le_bytes());
    }
    fn u64(&mut self, v: u64) {
        self.0.extend(v.to_le_bytes());
    }
    fn bytes(&mut self, b: &[u8]) {
        self.u64(b.len() as u64);
        self.0.extend_from_slice(b);
    }
    fn str(&mut self, s: &str) {
        self.bytes(s.as_bytes());
    }
    fn bool(&mut self, b: bool) {
        self.u8(u8::from(b));
    }
    fn dur(&mut self, d: Duration) {
        self.u64(d.as_nanos() as u64);
    }
    fn opt_u64(&mut self, v: Option<u64>) {
        match v {
            Some(v) => {
                self.u8(1);
                self.u64(v);
            }
            None => self.u8(0),
        }
    }
}

struct Dec<'a>(&'a [u8]);

impl Dec<'_> {
    fn take(&mut self, n: usize) -> Option<&[u8]> {
        if self.0.len() < n {
            return None;
        }
        let (a, b) = self.0.split_at(n);
        self.0 = b;
        Some(a)
    }
    fn u8(&mut self) -> Option<u8> {
        Some(self.take(1)?[0])
    }
    fn u16(&mut self) -> Option<u16> {
        Some(u16::from_le_bytes(self.take(2)?.try_into().ok()?))
    }
    fn u64(&mut self) -> Option<u64> {
        Some(u64::from_le_bytes(self.take(8)?.try_into().ok()?))
    }
    fn bytes(&mut self) -> Option<Bytes> {
        let n = usize::try_from(self.u64()?).ok()?;
        Some(Bytes::copy_from_slice(self.take(n)?))
    }
    fn str(&mut self) -> Option<String> {
        String::from_utf8(self.bytes()?.to_vec()).ok()
    }
    fn bool(&mut self) -> Option<bool> {
        Some(self.u8()? != 0)
    }
    fn dur(&mut self) -> Option<Duration> {
        Some(Duration::from_nanos(self.u64()?))
    }
    fn opt_u64(&mut self) -> Option<Option<u64>> {
        Some(match self.u8()? {
            0 => None,
            _ => Some(self.u64()?),
        })
    }
}

fn header_bytes(h: &LogHeader) -> Vec<u8> {
    let mut e = Enc(Vec::new());
    e.u16(FORMAT_VERSION);
    e.str(&h.engine);
    e.str(&h.game);
    e.u64(h.room);
    e.u64(h.seed);
    e.u64(h.tick_hz.to_bits());
    e.u64(u64::from(h.call_share));
    e.u64(h.max_pending_requests as u64);
    e.u64(h.max_pending_requests_per_conn as u64);
    e.dur(h.request_timeout);
    let mut out = MAGIC.to_vec();
    out.extend((e.0.len() as u32).to_le_bytes());
    out.extend(e.0);
    out
}

fn read_header(d: &mut Dec<'_>) -> Result<LogHeader, ReadError> {
    let format = d.u16().ok_or(ReadError::Corrupt("header"))?;
    if format != FORMAT_VERSION {
        return Err(ReadError::Version(format));
    }
    let h = (|| {
        Some(LogHeader {
            format,
            engine: d.str()?,
            game: d.str()?,
            room: d.u64()?,
            seed: d.u64()?,
            tick_hz: f64::from_bits(d.u64()?),
            call_share: u32::try_from(d.u64()?).ok()?,
            max_pending_requests: usize::try_from(d.u64()?).ok()?,
            max_pending_requests_per_conn: usize::try_from(d.u64()?).ok()?,
            request_timeout: d.dur()?,
        })
    })();
    h.ok_or(ReadError::Corrupt("header"))
}

fn write_frame(
    w: &mut impl Write,
    t: &TickRecord,
    codecs: &Codecs,
    stats: &mut WriterStats,
) -> io::Result<()> {
    let mut e = Enc(Vec::new());
    e.u64(t.tick);
    e.dur(t.dt);
    e.opt_u64(t.hash);
    e.u64(t.inputs.len() as u64);
    for i in &t.inputs {
        input(&mut e, i, codecs, stats);
    }
    w.write_all(&(e.0.len() as u32).to_le_bytes())?;
    w.write_all(&e.0)?;
    stats.frames_written += 1;
    Ok(())
}

// Input tags.
const JOIN: u8 = 1;
const RESUME: u8 = 2;
const LEAVE: u8 = 3;
const DISCONNECT: u8 = 4;
const RELEASE: u8 = 5;
const EXPIRE: u8 = 6;
const BATCH_DROPPED: u8 = 7;
const BATCH_RESUMED: u8 = 8;
const SESSION_END: u8 = 9;
const RESPONSES: u8 = 10;
const READ: u8 = 11;
const ACTIONS: u8 = 12;
const REQUEST: u8 = 13;
const SYSTEMS: u8 = 14;
const END_TICK: u8 = 15;
const SESSION_PARK: u8 = 16;
const SESSION_MOVE: u8 = 17;
/// Additive (round e5): a reader of an older log never meets it, so
/// `FORMAT_VERSION` stays (the q5 rule for tags 16, 17).
const EVENTS: u8 = 18;

fn input(e: &mut Enc, i: &Input, codecs: &Codecs, stats: &mut WriterStats) {
    match i {
        Input::Join {
            conn,
            identity,
            claims,
            player,
            entity,
        } => {
            e.u8(JOIN);
            e.u64(conn.0);
            e.str(identity);
            match claims {
                Some(c) => {
                    e.u8(1);
                    e.bytes(c);
                }
                None => e.u8(0),
            }
            e.u64(player.0);
            e.u64(*entity);
        }
        Input::Resume {
            identity,
            conn,
            player,
            entity,
        } => {
            e.u8(RESUME);
            e.str(identity);
            e.u64(conn.0);
            e.u64(player.0);
            e.u64(*entity);
        }
        Input::Leave { player } => {
            e.u8(LEAVE);
            e.u64(player.0);
        }
        Input::Disconnect {
            player,
            identity,
            cause,
        } => {
            e.u8(DISCONNECT);
            e.u64(player.0);
            e.str(identity);
            match cause {
                DisconnectCause::IdleInput => e.u8(1),
                DisconnectCause::Kicked => e.u8(2),
                DisconnectCause::ConnectionClosedBy(v) => {
                    e.u8(3);
                    e.u8(v.index() as u8);
                }
                // ConnectionClosed, and any later cause (written as the
                // plain close).
                _ => e.u8(0),
            }
        }
        Input::Release { player, released } => {
            e.u8(RELEASE);
            e.u64(player.0);
            e.bool(*released);
        }
        Input::Expire { player, to } => {
            e.u8(EXPIRE);
            e.u64(player.0);
            e.bool(matches!(to, ExpireTo::AiHandover));
        }
        Input::BatchDropped { player, snapshot } => {
            e.u8(BATCH_DROPPED);
            e.u64(player.0);
            e.bool(*snapshot);
        }
        Input::BatchResumed { player } => {
            e.u8(BATCH_RESUMED);
            e.u64(player.0);
        }
        Input::SessionEnd { conn } => {
            e.u8(SESSION_END);
            e.u64(conn.0);
        }
        Input::SessionPark { conn } => {
            e.u8(SESSION_PARK);
            e.u64(conn.0);
        }
        Input::SessionMove { from, to } => {
            e.u8(SESSION_MOVE);
            e.u64(from.0);
            e.u64(to.0);
        }
        Input::Responses(list) => {
            e.u8(RESPONSES);
            e.u64(list.len() as u64);
            for r in list {
                e.u64(r.key.origin);
                e.u64(r.key.seq);
                e.str(&r.call);
                e.u8(match r.arrival {
                    Arrival::Ended => 0,
                    Arrival::Deadline => 1,
                    Arrival::Late => 2,
                });
                e.bool(r.alive);
                let body = match &r.outcome {
                    RecordedOutcome::Value(o) => {
                        let b = codecs.encode(&r.call, o);
                        if b.is_none() {
                            stats.outcomes_unencodable += 1;
                        }
                        b.map(Bytes::from)
                    }
                    RecordedOutcome::Encoded(b) => Some(b.clone()),
                    RecordedOutcome::Missing => None,
                };
                match body {
                    Some(b) => {
                        e.u8(1);
                        e.bytes(&b);
                    }
                    None => e.u8(0),
                }
            }
        }
        Input::Read {
            conn,
            player,
            from,
            owner,
            wire_id,
            op,
            payload,
        } => {
            e.u8(READ);
            e.u64(conn.0);
            e.u64(player.0);
            e.str(from.as_str());
            e.opt_u64(owner.map(|o| o.0));
            e.u64(*wire_id);
            e.u16(*op);
            e.bytes(payload);
        }
        Input::Events(list) => {
            e.u8(EVENTS);
            e.u64(list.len() as u64);
            for ev in list {
                match &ev.to {
                    Some(id) => {
                        e.u8(1);
                        e.str(id.as_str());
                    }
                    None => e.u8(0),
                }
                e.u64(ev.owner.0);
                e.u64(u64::from(ev.publisher.0));
                e.u64(ev.seq);
                e.u16(ev.op);
                e.bytes(&ev.payload);
            }
        }
        Input::Actions(list) => {
            e.u8(ACTIONS);
            e.u64(list.len() as u64);
            for a in list {
                e.u64(a.conn.0);
                e.u64(a.player.0);
                e.u16(a.op);
                e.bytes(&a.payload);
            }
        }
        Input::Request {
            conn,
            player,
            id,
            op,
            payload,
        } => {
            e.u8(REQUEST);
            e.u64(conn.0);
            e.u64(player.0);
            e.u64(*id);
            e.u16(*op);
            e.bytes(payload);
        }
        Input::Systems => e.u8(SYSTEMS),
        Input::EndTick => e.u8(END_TICK),
    }
}

fn read_frame(d: &mut Dec<'_>) -> Option<TickRecord> {
    let tick = d.u64()?;
    let dt = d.dur()?;
    let hash = d.opt_u64()?;
    let n = d.u64()?;
    let mut inputs = Vec::new();
    for _ in 0..n {
        inputs.push(read_input(d)?);
    }
    if !d.0.is_empty() {
        return None;
    }
    Some(TickRecord {
        tick,
        dt,
        inputs,
        hash,
    })
}

fn read_input(d: &mut Dec<'_>) -> Option<Input> {
    Some(match d.u8()? {
        JOIN => Input::Join {
            conn: ConnectionId(d.u64()?),
            identity: d.str()?,
            claims: match d.u8()? {
                0 => None,
                _ => Some(d.bytes()?),
            },
            player: PlayerId(d.u64()?),
            entity: d.u64()?,
        },
        RESUME => Input::Resume {
            identity: d.str()?,
            conn: ConnectionId(d.u64()?),
            player: PlayerId(d.u64()?),
            entity: d.u64()?,
        },
        LEAVE => Input::Leave {
            player: PlayerId(d.u64()?),
        },
        DISCONNECT => Input::Disconnect {
            player: PlayerId(d.u64()?),
            identity: d.str()?,
            cause: match d.u8()? {
                0 => DisconnectCause::ConnectionClosed,
                1 => DisconnectCause::IdleInput,
                2 => DisconnectCause::Kicked,
                3 => DisconnectCause::ConnectionClosedBy(
                    *ServerClose::ALL.get(usize::from(d.u8()?))?,
                ),
                _ => return None,
            },
        },
        RELEASE => Input::Release {
            player: PlayerId(d.u64()?),
            released: d.bool()?,
        },
        EXPIRE => Input::Expire {
            player: PlayerId(d.u64()?),
            to: if d.bool()? {
                ExpireTo::AiHandover
            } else {
                ExpireTo::Despawn
            },
        },
        BATCH_DROPPED => Input::BatchDropped {
            player: PlayerId(d.u64()?),
            snapshot: d.bool()?,
        },
        BATCH_RESUMED => Input::BatchResumed {
            player: PlayerId(d.u64()?),
        },
        SESSION_END => Input::SessionEnd {
            conn: ConnectionId(d.u64()?),
        },
        SESSION_PARK => Input::SessionPark {
            conn: ConnectionId(d.u64()?),
        },
        SESSION_MOVE => Input::SessionMove {
            from: ConnectionId(d.u64()?),
            to: ConnectionId(d.u64()?),
        },
        RESPONSES => {
            let n = d.u64()?;
            let mut list = Vec::new();
            for _ in 0..n {
                let key = CallKey {
                    origin: d.u64()?,
                    seq: d.u64()?,
                };
                let call = Cow::Owned(d.str()?);
                let arrival = match d.u8()? {
                    0 => Arrival::Ended,
                    1 => Arrival::Deadline,
                    2 => Arrival::Late,
                    _ => return None,
                };
                let alive = d.bool()?;
                let outcome = match d.u8()? {
                    0 => RecordedOutcome::Missing,
                    _ => RecordedOutcome::Encoded(d.bytes()?),
                };
                list.push(Recorded {
                    key,
                    call,
                    arrival,
                    alive,
                    outcome,
                });
            }
            Input::Responses(list)
        }
        READ => Input::Read {
            conn: ConnectionId(d.u64()?),
            player: PlayerId(d.u64()?),
            from: Identity::new(&d.str()?),
            owner: d.opt_u64()?.map(Owner),
            wire_id: d.u64()?,
            op: d.u16()?,
            payload: d.bytes()?,
        },
        EVENTS => {
            let n = d.u64()?;
            let mut list = Vec::new();
            for _ in 0..n {
                list.push(RecordedEvent {
                    to: match d.u8()? {
                        0 => None,
                        _ => Some(Identity::new(&d.str()?)),
                    },
                    owner: Owner(d.u64()?),
                    publisher: PublisherId(u32::try_from(d.u64()?).ok()?),
                    seq: d.u64()?,
                    op: d.u16()?,
                    payload: d.bytes()?,
                });
            }
            Input::Events(list)
        }
        ACTIONS => {
            let n = d.u64()?;
            let mut list = Vec::new();
            for _ in 0..n {
                list.push(RecordedAction {
                    conn: ConnectionId(d.u64()?),
                    player: PlayerId(d.u64()?),
                    op: d.u16()?,
                    payload: d.bytes()?,
                });
            }
            Input::Actions(list)
        }
        REQUEST => Input::Request {
            conn: ConnectionId(d.u64()?),
            player: PlayerId(d.u64()?),
            id: d.u64()?,
            op: d.u16()?,
            payload: d.bytes()?,
        },
        SYSTEMS => Input::Systems,
        END_TICK => Input::EndTick,
        _ => return None,
    })
}
