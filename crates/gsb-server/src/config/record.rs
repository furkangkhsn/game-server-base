//! `[record]` (BACKLOG B170, docs/SERVICES.md §10.1): which rooms write
//! their input log, where, under which game version. The table PARSES in
//! every build, so a build without the `record` cargo feature refuses it
//! at startup by name (`ServerError::RecordNotBuilt`) instead of
//! silently recording nothing.

/// `[record]`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecordSection {
    /// The directory the log files go to (created when missing; one file
    /// per room incarnation, `room-<id>-<generation>.gsblog`).
    pub dir: String,
    /// The room ids recorded; absent = every room the server hosts.
    /// Sharded rooms are not recorded (a warning; BACKLOG B171).
    #[serde(default)]
    pub rooms: Option<Vec<u64>>,
    /// The game's version in every log's header (a replay needs the same
    /// game build); default: the game's name.
    #[serde(default)]
    pub game_version: Option<String>,
    /// Each room's record channel, in tick records (default 4096; `0`
    /// refuses startup): the slack between the tick and the writer
    /// thread. A full channel loses the record and counts it
    /// (`records_dropped`).
    #[serde(default = "default_buffer")]
    pub buffer: usize,
}

fn default_buffer() -> usize {
    4096
}
