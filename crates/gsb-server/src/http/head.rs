//! Reading the one request head a connection sends: bounded in size
//! (`MAX_HEAD_BYTES`, 431) and in time (`HEAD_DEADLINE`, 408).

use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt};

/// Request-head cap in bytes. A head that outgrows it is answered 431 and
/// dropped: the surface serves operators, not uploads, and an unbounded
/// read would be a memory-amplification bug for anyone who can reach the
/// port.
const MAX_HEAD_BYTES: usize = 8 * 1024;

/// How long a connection has to deliver its whole request head (BACKLOG
/// B47). Past it the peer is answered 408 and closed. WHY: the head read
/// is the one wait a peer controls; without a bound, a peer that connects
/// and sends nothing (or dribbles a byte at a time — the deadline covers
/// the whole head, not each read) keeps its connection task alive until
/// it disconnects. A scraper or `curl` sends its head in one write right
/// after connecting, so 5 s is far past any honest client, even over a
/// slow link, and short enough that such a task dies quickly.
pub(super) const HEAD_DEADLINE: Duration = Duration::from_secs(5);

/// Why no request head came.
pub(super) enum HeadError {
    Malformed,
    TooLarge,
    /// `HEAD_DEADLINE` passed before the head ended.
    TimedOut,
}

/// [`read_head`] under [`HEAD_DEADLINE`]: one timeout around the whole
/// read (no multiplexed wait — the read is the only thing awaited).
pub(super) async fn read_head_in_time<S: AsyncRead + Unpin>(
    stream: &mut S,
) -> Result<String, HeadError> {
    tokio::time::timeout(HEAD_DEADLINE, read_head(stream))
        .await
        .unwrap_or(Err(HeadError::TimedOut))
}

/// Read exactly one request head: bytes up to (not including) the CRLFCRLF
/// terminator. Any body is ignored by construction (we stop reading at the
/// terminator and answer `Connection: close`).
async fn read_head<S: AsyncRead + Unpin>(stream: &mut S) -> Result<String, HeadError> {
    let mut buf: Vec<u8> = Vec::with_capacity(512);
    let mut chunk = [0u8; 1024];
    loop {
        if let Some(end) = find_head_end(&buf) {
            return String::from_utf8(buf[..end].to_vec()).map_err(|_| HeadError::Malformed);
        }
        if buf.len() > MAX_HEAD_BYTES {
            return Err(HeadError::TooLarge);
        }
        let n = stream
            .read(&mut chunk)
            .await
            .map_err(|_| HeadError::Malformed)?;
        if n == 0 {
            // EOF before the head ended: not a request.
            return Err(HeadError::Malformed);
        }
        buf.extend_from_slice(&chunk[..n]);
    }
}

fn find_head_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
}
