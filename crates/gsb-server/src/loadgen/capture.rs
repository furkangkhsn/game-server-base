//! `--capture DIR` (KIT-ARCHITECTURE §10 "A22"): the raw game-band
//! frames a sample of clients received, written as one file per client
//! for an offline wire study (byte anatomy, replaying candidate
//! encodings). A measurement aid: it changes nothing a client sends or
//! applies — the frame is recorded next to being applied, in memory
//! during the run, and written once when the client's session ends.
//!
//! File format (`client-<id>.gsbcap`, little-endian):
//!
//! ```text
//! b"GSBCAP1\n"  u64 client id  u16 game-name length  game name (UTF-8)
//! then per received frame, in arrival order:
//!   u8 kind (0 = the game's group snapshot op, 1 = its private op,
//!            2 = the core's JOIN_ROOM_RESULT — the client's own wire id)
//!   u32 milliseconds since the client's capture began
//!   u32 payload length   payload (the frame body, without the core's
//!                        length/opcode header)
//! ```

use std::path::PathBuf;
use std::time::Instant;

/// The file's first eight bytes.
pub(crate) const MAGIC: &[u8; 8] = b"GSBCAP1\n";

/// What a captured frame was.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum Kind {
    /// The game's group snapshot opcode (`Game::SNAPSHOT_OP`).
    Snapshot = 0,
    /// The game's private opcode (`Game::PRIVATE_OP`).
    Private = 1,
    /// The core's `JOIN_ROOM_RESULT` (`JoinRoomResult { entity }`): which
    /// record in the view is the client's own.
    Joined = 2,
}

/// One client's capture: the frames so far, and where they go.
pub(crate) struct Capture {
    path: PathBuf,
    buf: Vec<u8>,
    t0: Instant,
}

impl Capture {
    /// A capture of client `id` of `game`, to be written to `path`.
    pub(crate) fn new(path: PathBuf, game: &str, id: u64) -> Self {
        let mut buf = Vec::with_capacity(1 << 16);
        buf.extend_from_slice(MAGIC);
        buf.extend_from_slice(&id.to_le_bytes());
        let name = game.as_bytes();
        let len = u16::try_from(name.len()).expect("a game name fits in u16");
        buf.extend_from_slice(&len.to_le_bytes());
        buf.extend_from_slice(name);
        Self {
            path,
            buf,
            t0: Instant::now(),
        }
    }

    /// Record one received frame body.
    pub(crate) fn frame(&mut self, kind: Kind, payload: &[u8]) {
        let ms = u32::try_from(self.t0.elapsed().as_millis()).unwrap_or(u32::MAX);
        let len = u32::try_from(payload.len()).expect("a frame fits in u32");
        self.buf.push(kind as u8);
        self.buf.extend_from_slice(&ms.to_le_bytes());
        self.buf.extend_from_slice(&len.to_le_bytes());
        self.buf.extend_from_slice(payload);
    }

    /// Write the file (off the runtime's workers: one blocking write at
    /// the end of the client's session).
    pub(crate) async fn finish(self) {
        let Self { path, buf, .. } = self;
        let shown = path.display().to_string();
        let written = tokio::task::spawn_blocking(move || std::fs::write(&path, &buf)).await;
        match written {
            Ok(Ok(())) => {}
            Ok(Err(e)) => eprintln!("--capture: cannot write {shown}: {e}"),
            Err(e) => eprintln!("--capture: writer task for {shown} failed: {e}"),
        }
    }
}

/// Whether client number `i` of `n` (0-based, before `--offset`) is one
/// of the `k` captured: `k` ids spread evenly over the run (every
/// `n / k`-th), so a sample covers different groups (teams, cells).
pub(crate) fn captured(i: u64, n: u64, k: u64) -> bool {
    let stride = (n / k.max(1)).max(1);
    i.is_multiple_of(stride) && i / stride < k
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `k` clients are picked, spread over the run; never more than `n`.
    #[test]
    fn the_sample_is_spread_and_bounded() {
        let picked = |n, k| (0..n).filter(|&i| captured(i, n, k)).collect::<Vec<_>>();
        assert_eq!(picked(10, 3), [0, 3, 6]);
        assert_eq!(picked(500, 8), [0, 62, 124, 186, 248, 310, 372, 434]);
        assert_eq!(picked(3, 8), [0, 1, 2]);
        assert!(picked(5, 0).is_empty());
    }

    /// The header, then one record per frame in arrival order.
    #[test]
    fn the_file_layout_is_the_documented_one() {
        let mut c = Capture::new(PathBuf::from("unused"), "war", 7);
        c.frame(Kind::Snapshot, &[1, 2, 3]);
        c.frame(Kind::Private, &[]);
        let b = &c.buf;
        assert_eq!(&b[..8], MAGIC);
        assert_eq!(u64::from_le_bytes(b[8..16].try_into().unwrap()), 7);
        assert_eq!(u16::from_le_bytes(b[16..18].try_into().unwrap()), 3);
        assert_eq!(&b[18..21], b"war");
        let r = &b[21..];
        assert_eq!(r[0], 0);
        assert_eq!(u32::from_le_bytes(r[5..9].try_into().unwrap()), 3);
        assert_eq!(&r[9..12], [1, 2, 3]);
        let r = &r[12..];
        assert_eq!(r[0], 1);
        assert_eq!(u32::from_le_bytes(r[5..9].try_into().unwrap()), 0);
        assert_eq!(r.len(), 9);
    }
}
