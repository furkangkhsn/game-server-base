//! Byte layout of the future SEALED datagram (not wired yet — B5a).
//!
//! ```text
//! c→s  [kind 1][cid u64 LE 8][counter u64 LE 8][ciphertext ..][tag 16]   overhead 33 B
//! s→c  [kind 1]              [counter u64 LE 8][ciphertext ..][tag 16]   overhead 25 B
//! ```
//!
//! The header (kind, CID, counter) is the AEAD's associated data: it
//! travels in the clear but any change to it fails authentication. The
//! client never sees its NAT rebind, so EVERY c→s datagram names its
//! session by CID; s→c needs none (QUIC's zero-length CID idea).

/// Kind byte of a SEALED datagram, key phase bit clear. Plaintext kinds
/// use 0..=8 today and B3 reserves the 0x80 bit as its CID tag, so 0x40
/// collides with neither; B5a confirms the final value with B3's layout.
pub const KIND_SEALED: u8 = 0x40;
/// The key phase bit inside the kind byte (QUIC's KEY_PHASE idea).
pub const KIND_PHASE_BIT: u8 = 0x01;
/// Bytes of the kind field.
pub const KIND_LEN: usize = 1;
/// Bytes of the connection ID (c→s only).
pub const CID_LEN: usize = 8;
/// Bytes of the record counter (the AEAD nonce, sent in full).
pub const COUNTER_LEN: usize = 8;
/// Bytes of the Poly1305 tag that ends every sealed datagram.
pub const TAG_LEN: usize = 16;
/// Header bytes of a c→s datagram: kind + CID + counter.
pub const HEADER_LEN_C2S: usize = KIND_LEN + CID_LEN + COUNTER_LEN;
/// Header bytes of a s→c datagram: kind + counter.
pub const HEADER_LEN_S2C: usize = KIND_LEN + COUNTER_LEN;
/// Total bytes a c→s seal adds around the plaintext.
pub const OVERHEAD_C2S: usize = HEADER_LEN_C2S + TAG_LEN;
/// Total bytes a s→c seal adds around the plaintext.
pub const OVERHEAD_S2C: usize = HEADER_LEN_S2C + TAG_LEN;

/// Which way a datagram travels; decides whether the header has a CID.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    /// Client to server: the header carries the CID.
    ClientToServer,
    /// Server to client: no CID.
    ServerToClient,
}

impl Direction {
    /// Header bytes in this direction.
    pub const fn header_len(self) -> usize {
        match self {
            Direction::ClientToServer => HEADER_LEN_C2S,
            Direction::ServerToClient => HEADER_LEN_S2C,
        }
    }
}

/// The key phase a datagram was sealed under: the low bit of the key
/// generation (generation 0, 2, 4.. → `Zero`; 1, 3, 5.. → `One`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyPhase {
    /// Even key generation.
    Zero,
    /// Odd key generation.
    One,
}

impl KeyPhase {
    /// The phase of key generation `generation`.
    pub const fn of(generation: u64) -> Self {
        if generation & 1 == 0 {
            KeyPhase::Zero
        } else {
            KeyPhase::One
        }
    }
}

/// A parsed SEALED header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    /// Key phase bit.
    pub phase: KeyPhase,
    /// The session's CID; `Some` exactly for c→s.
    pub cid: Option<u64>,
    /// The record counter.
    pub counter: u64,
}

impl Header {
    /// Encodes into a fixed buffer; returns it and the used length.
    pub fn encode(&self) -> ([u8; HEADER_LEN_C2S], usize) {
        let mut b = [0u8; HEADER_LEN_C2S];
        b[0] = match self.phase {
            KeyPhase::Zero => KIND_SEALED,
            KeyPhase::One => KIND_SEALED | KIND_PHASE_BIT,
        };
        let mut at = KIND_LEN;
        if let Some(cid) = self.cid {
            b[at..at + CID_LEN].copy_from_slice(&cid.to_le_bytes());
            at += CID_LEN;
        }
        b[at..at + COUNTER_LEN].copy_from_slice(&self.counter.to_le_bytes());
        (b, at + COUNTER_LEN)
    }

    /// Parses the header of a whole datagram travelling `dir`. `None` if
    /// the kind is not SEALED or the datagram cannot even hold a header
    /// and a tag (an empty plaintext is allowed).
    pub fn decode(d: &[u8], dir: Direction) -> Option<Self> {
        if d.len() < dir.header_len() + TAG_LEN || d[0] & !KIND_PHASE_BIT != KIND_SEALED {
            return None;
        }
        let phase = KeyPhase::of(u64::from(d[0] & KIND_PHASE_BIT));
        let u64_at = |at: usize| {
            let mut w = [0u8; 8];
            w.copy_from_slice(&d[at..at + 8]);
            u64::from_le_bytes(w)
        };
        let (cid, at) = match dir {
            Direction::ClientToServer => (Some(u64_at(KIND_LEN)), KIND_LEN + CID_LEN),
            Direction::ServerToClient => (None, KIND_LEN),
        };
        Some(Header {
            phase,
            cid,
            counter: u64_at(at),
        })
    }
}
