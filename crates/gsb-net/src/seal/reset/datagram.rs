//! The stateless reset datagram's layout (B5b; RUDP-SECURITY §8).
//!
//! ```text
//! [kind 0x40 | random phase bit][random counter < 2^62, u64 LE][random ..][token 16]
//! ```
//!
//! Shaped like a server → client SEALED record (RFC 9000 §10.3's idea):
//! the SEALED kind byte with a random phase bit, a counter-sized field,
//! random bytes where a ciphertext would be, and 16 bytes where a tag
//! would be. A client opens it as a record first; the AEAD refuses it
//! (the counter stays below 2^62 so it reaches the AEAD instead of being
//! refused as malformed), and only then are its last 16 bytes compared
//! with the session's token, in constant time.
//!
//! - **Never an amplifier:** the reset is strictly shorter than the
//!   datagram that triggered it ([`reset_datagram`]), and at most
//!   [`RESET_LEN_MAX`] bytes. Two endpoints that each take the other's
//!   datagram for an unknown session run down by a byte a round and stop
//!   below a c→s record's minimum size (33 B), within ten rounds.
//! - **What it does not hide:** the counter travels in the clear (§10's
//!   counter masking is later), so an observer that tracks a session's
//!   counters sees a random one; the shape, size range and body are a
//!   small record's.

use super::*;

/// The shortest reset: one byte more than an empty SEALED s→c record
/// (25 B), so the token never overlaps the counter.
pub const RESET_LEN_MIN: usize = OVERHEAD_S2C + 1;
/// The longest reset: the size of a small sealed control record (an
/// ACK is 30 B, a probe or a path challenge 34). A longer trigger gets
/// this size — a reset needs no more, and a smaller answer is cheaper.
pub const RESET_LEN_MAX: usize = OVERHEAD_S2C + 16;

/// The reset answering a trigger of `trigger_len` bytes: `min(trigger_len
/// − 1, RESET_LEN_MAX)` bytes — always SHORTER than the trigger — or
/// `None` when that is below [`RESET_LEN_MIN`] (no reset can be both
/// shorter and well-formed). `random`: fresh random bytes (the phase bit,
/// the counter, the filler).
pub fn reset_datagram(
    token: &ResetToken,
    trigger_len: usize,
    random: &[u8; RESET_LEN_MAX],
) -> Option<Vec<u8>> {
    let len = trigger_len.checked_sub(1)?.min(RESET_LEN_MAX);
    if len < RESET_LEN_MIN {
        return None;
    }
    let mut d = random[..len - RESET_TOKEN_LEN].to_vec();
    d[0] = KIND_SEALED | (random[0] & KIND_PHASE_BIT);
    // The counter's top byte: below 2^62, like every real counter.
    d[HEADER_LEN_S2C - 1] &= 0x3F;
    d.extend_from_slice(&token.0);
    Some(d)
}

/// The would-be token of a server → client datagram that may be a
/// stateless reset — a SEALED kind byte and a reset's length — or `None`
/// (it cannot be one).
pub fn reset_tail(d: &[u8]) -> Option<&[u8]> {
    let sealed = d
        .first()
        .is_some_and(|k| k & !KIND_PHASE_BIT == KIND_SEALED);
    let sized = (RESET_LEN_MIN..=RESET_LEN_MAX).contains(&d.len());
    (sealed && sized).then(|| &d[d.len() - RESET_TOKEN_LEN..])
}
