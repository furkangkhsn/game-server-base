//! Recording rooms from the registry (feature `record`, BACKLOG B170,
//! `docs/SERVICES.md` §10.1): a [`RecordPlan`] names a directory, the
//! rooms to record and the game's version; every install of a selected
//! single room — its first create, a re-create, a rebuild after a panic
//! — gets a [`Recorder`] and a writer thread of its own, writing one log
//! file per incarnation (`room-<id>-<generation>.gsblog`). The tick never
//! touches the file: the room `try_send`s its records (a full or gone
//! sink counts `records_dropped`), the thread writes them and ends when
//! the room's recorder drops (the room ended). Sharded rooms are not
//! recorded (BACKLOG B171): selecting one warns and records nothing.

use std::path::PathBuf;

use tracing::{info, warn};

use crate::determinism::Recorder;
use crate::id::RoomId;

/// Which rooms a [`RecordPlan`] records.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecordRooms {
    /// Every room the registry installs.
    All,
    /// Only these ids.
    Only(Vec<RoomId>),
}

/// What a registry records, and where ([`Registry::with_record`](crate::registry::Registry::with_record)).
#[derive(Debug, Clone)]
pub struct RecordPlan {
    /// The directory the log files go to (it must exist).
    pub dir: PathBuf,
    /// The rooms recorded.
    pub rooms: RecordRooms,
    /// The game's version, in every log's header (a replay needs the same
    /// game build).
    pub game: String,
    /// Each room's record channel, in tick records (the writer's slack: a
    /// full channel loses the record and counts it, `records_dropped`).
    pub buffer: usize,
}

/// The default record channel ([`RecordPlan::buffer`]): about a minute
/// of 60 Hz ticks.
pub const DEFAULT_RECORD_BUFFER: usize = 4096;

impl RecordPlan {
    /// A plan recording every room into `dir` as `game`.
    pub fn new(dir: impl Into<PathBuf>, game: &str) -> Self {
        Self {
            dir: dir.into(),
            rooms: RecordRooms::All,
            game: game.to_owned(),
            buffer: DEFAULT_RECORD_BUFFER,
        }
    }

    /// Whether `room` is recorded.
    pub fn records(&self, room: RoomId) -> bool {
        match &self.rooms {
            RecordRooms::All => true,
            RecordRooms::Only(ids) => ids.contains(&room),
        }
    }

    /// The log file of `room`'s incarnation `generation`.
    pub fn path(&self, room: RoomId, generation: u64) -> PathBuf {
        self.dir
            .join(format!("room-{}-{generation}.gsblog", room.0))
    }

    /// A recorder for `room`'s incarnation `generation`, its writer thread
    /// started (it logs how the file ended).
    pub(crate) fn start(&self, room: RoomId, generation: u64) -> Recorder {
        let (recorder, mut inbox) = Recorder::channel(self.buffer, &self.game);
        let path = self.path(room, generation);
        std::thread::spawn(move || {
            match crate::determinism::file::write_records(&path, &mut inbox) {
                Ok(s) => {
                    info!(
                        room = room.0,
                        path = %path.display(),
                        frames = s.frames_written,
                        "input log written"
                    );
                    if s.outcomes_unencodable > 0 || s.frames_headless > 0 {
                        warn!(
                            room = room.0,
                            path = %path.display(),
                            outcomes_unencodable = s.outcomes_unencodable,
                            frames_headless = s.frames_headless,
                            "input log incomplete: outcomes without a codec are written \
                             missing (Calls::record_as / record_service)"
                        );
                    }
                }
                // The inbox drops with the thread: the room counts every
                // later record `records_dropped`.
                Err(e) => warn!(
                    room = room.0,
                    path = %path.display(),
                    error = %e,
                    "input log writer failed; the room's later records are dropped"
                ),
            }
        });
        recorder
    }
}
