//! The receive half: replay window, key phases, forgery budget.

use super::key::PhaseKey;
use super::replay::{Check, REPLAY_WINDOW, ReplayWindow};
use super::sealer::SEAL_LIMIT;
use super::wire::{Direction, Header, KeyPhase};

/// Datagrams that may fail authentication over a session's lifetime,
/// across all keys: RFC 9001 §6.6's integrity limit for
/// AEAD_CHACHA20_POLY1305 (2^36). Past it every open is refused with
/// [`Refusal::IntegrityLimit`] and the session must be closed.
pub const INTEGRITY_LIMIT: u64 = 1 << 36;

/// Why a datagram was refused. Every refusal is exactly one of these, so
/// the caller counts each loss under its exact name. Checks run in this
/// order, cheapest first; the first that fires names the datagram.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Refusal {
    /// The session already exceeded [`INTEGRITY_LIMIT`]; nothing is opened.
    IntegrityLimit,
    /// Not a SEALED datagram of this direction, too short for header and
    /// tag, or a counter at/over the sealer's limit.
    Malformed,
    /// Older than the replay window (the datagram may well be genuine).
    TooOld,
    /// The counter was already opened (inside the window).
    Replayed,
    /// The phase bit contradicts the counter: a new phase below a counter
    /// already opened under the current one. No honest peer sends it.
    WrongPhase,
    /// Authentication failed: forged, corrupted, or the wrong key.
    Forged,
}

impl Refusal {
    /// Every refusal, for registering one counter per name.
    pub const ALL: [Refusal; 6] = [
        Refusal::IntegrityLimit,
        Refusal::Malformed,
        Refusal::TooOld,
        Refusal::Replayed,
        Refusal::WrongPhase,
        Refusal::Forged,
    ];

    /// A stable counter name.
    pub const fn name(self) -> &'static str {
        match self {
            Refusal::IntegrityLimit => "seal_integrity_limit",
            Refusal::Malformed => "seal_malformed",
            Refusal::TooOld => "seal_too_old",
            Refusal::Replayed => "seal_replayed",
            Refusal::WrongPhase => "seal_wrong_phase",
            Refusal::Forged => "seal_forged",
        }
    }
}

/// An authenticated datagram.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Opened {
    /// The inner datagram, exactly as the peer passed it to `seal`.
    pub plaintext: Vec<u8>,
    /// Its record counter.
    pub counter: u64,
    /// Higher than every counter opened before: with authentication and
    /// path validation, the third condition for following an address
    /// change (RFC 9146 §6).
    pub newest: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Which {
    Prev,
    Cur,
    Next,
}

/// Opens datagrams in one direction. Lives in the receive path (the
/// demux on the server); shares nothing with the [`Sealer`](super::Sealer).
pub struct Opener {
    dir: Direction,
    generation: u64,
    cur: PhaseKey,
    /// `REKEY(cur)`, tried when the phase bit flips forward.
    next: PhaseKey,
    /// The previous generation, kept until its counters leave the window.
    prev: Option<PhaseKey>,
    /// Lowest counter seen under `cur` when it was promoted (0 at start);
    /// every previous-phase counter is below it.
    cur_start: u64,
    window: ReplayWindow,
    forged: u64,
}

impl Opener {
    pub(super) fn new(raw: &mut [u8; 32], dir: Direction) -> Self {
        let cur = PhaseKey::new(raw);
        let next = cur.rekey();
        Opener {
            dir,
            generation: 0,
            cur,
            next,
            prev: None,
            cur_start: 0,
            window: ReplayWindow::new(),
            forged: 0,
        }
    }

    /// Authenticates one whole SEALED datagram.
    pub fn open(&mut self, datagram: &[u8]) -> Result<Opened, Refusal> {
        if self.forged > INTEGRITY_LIMIT {
            return Err(Refusal::IntegrityLimit);
        }
        let h = Header::decode(datagram, self.dir).ok_or(Refusal::Malformed)?;
        if h.counter >= SEAL_LIMIT {
            return Err(Refusal::Malformed);
        }
        match self.window.check(h.counter) {
            Check::TooOld => return Err(Refusal::TooOld),
            Check::Replayed => return Err(Refusal::Replayed),
            Check::Fresh => {}
        }
        let top = self.window.top();
        let newest = top.is_none_or(|t| h.counter > t);
        let (key, which) = if h.phase == KeyPhase::of(self.generation) {
            (&self.cur, Which::Cur)
        } else if let Some(prev) = self.prev.as_ref().filter(|_| h.counter < self.cur_start) {
            (prev, Which::Prev)
        } else if newest {
            (&self.next, Which::Next)
        } else {
            return Err(Refusal::WrongPhase);
        };
        let (aad, body) = datagram.split_at(self.dir.header_len());
        let Some(plaintext) = key.open(h.counter, aad, body) else {
            self.forged += 1;
            return Err(Refusal::Forged);
        };
        self.window.mark(h.counter);
        if which == Which::Next {
            let after = self.next.rekey();
            let cur = std::mem::replace(&mut self.next, after);
            self.prev = Some(std::mem::replace(&mut self.cur, cur));
            self.generation += 1;
            self.cur_start = h.counter;
        }
        // Once the window passed `cur_start`, every previous-phase counter
        // reads TooOld: the old key can go.
        if let Some(top) = self.window.top()
            && top >= self.cur_start + (REPLAY_WINDOW - 1)
        {
            self.prev = None;
        }
        Ok(Opened {
            plaintext,
            counter: h.counter,
            newest,
        })
    }

    /// Datagrams that failed authentication so far (all keys).
    pub fn forged(&self) -> u64 {
        self.forged
    }

    /// The key generation of the latest phase the peer moved to.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Whether the previous generation's key is still held (the grace for
    /// reordered datagrams across a phase change).
    pub fn holds_previous_key(&self) -> bool {
        self.prev.is_some()
    }

    #[cfg(test)]
    pub(super) fn set_forged_for_test(&mut self, forged: u64) {
        self.forged = forged;
    }
}
