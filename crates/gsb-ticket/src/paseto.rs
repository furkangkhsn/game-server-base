//! PASETO v4.public: the token format (the PASETO specification,
//! `docs/Versions/v4.md` "Sign" / "Verify").
//!
//! ```text
//! token = "v4.public." base64url(m || sig) [ "." base64url(f) ]
//! sig   = Ed25519.Sign(sk, PAE("v4.public.", m, f, i))
//! ```
//!
//! `m` is the message (here: the claims' JSON), `f` the optional footer
//! (authenticated, not encrypted — here: `{"kid":"…"}`), `i` the implicit
//! assertion (authenticated, never transmitted — empty here). The version
//! and purpose are fixed by the header, never negotiated: there is no
//! `alg` field to confuse (the JWT failure mode PASETO exists to remove).
//!
//! Verification is `verify_strict`: a non-canonical signature or a
//! small-order public key is refused (no signature malleability).

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};

/// The header of every token this module reads or writes.
pub const HEADER: &str = "v4.public.";

/// An Ed25519 signature's length.
const SIG_LEN: usize = 64;

/// Why a token could not be read or verified.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// Not `v4.public.`, not base64url, too short, a wrong footer.
    Malformed,
    /// The signature does not verify under the key.
    Signature,
}

/// Pre-Authentication Encoding: the piece count, then each piece's
/// length and bytes, every length a little-endian `u64` with its top bit
/// clear.
pub fn pae(pieces: &[&[u8]]) -> Vec<u8> {
    let le64 = |n: usize| ((n as u64) & (u64::MAX >> 1)).to_le_bytes();
    let len = 8 + pieces.iter().map(|p| 8 + p.len()).sum::<usize>();
    let mut out = Vec::with_capacity(len);
    out.extend_from_slice(&le64(pieces.len()));
    for p in pieces {
        out.extend_from_slice(&le64(p.len()));
        out.extend_from_slice(p);
    }
    out
}

/// Sign `message` with `footer` and the implicit assertion `implicit`.
pub fn sign(key: &SigningKey, message: &[u8], footer: &[u8], implicit: &[u8]) -> String {
    let m2 = pae(&[HEADER.as_bytes(), message, footer, implicit]);
    let sig = key.sign(&m2);
    let mut body = Vec::with_capacity(message.len() + SIG_LEN);
    body.extend_from_slice(message);
    body.extend_from_slice(&sig.to_bytes());
    let mut token = String::with_capacity(HEADER.len() + body.len() * 4 / 3 + 4);
    token.push_str(HEADER);
    B64.encode_string(&body, &mut token);
    if !footer.is_empty() {
        token.push('.');
        B64.encode_string(footer, &mut token);
    }
    token
}

/// A token split into its parts, before any signature check: the signed
/// message, the signature and the footer (empty when absent).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Parts {
    pub message: Vec<u8>,
    signature: [u8; SIG_LEN],
    pub footer: Vec<u8>,
}

/// Split `token` (no verification yet: the footer is read first to pick
/// the key, and authenticated by [`Parts::verify`]).
pub fn split(token: &str) -> Result<Parts, Refusal> {
    let rest = token.strip_prefix(HEADER).ok_or(Refusal::Malformed)?;
    let (payload, footer) = match rest.split_once('.') {
        Some((p, f)) => (p, f),
        None => (rest, ""),
    };
    // One footer at most, and a present footer is never empty.
    if footer.contains('.') || (rest.contains('.') && footer.is_empty()) {
        return Err(Refusal::Malformed);
    }
    let body = B64.decode(payload).map_err(|_| Refusal::Malformed)?;
    let footer = B64.decode(footer).map_err(|_| Refusal::Malformed)?;
    if body.len() < SIG_LEN {
        return Err(Refusal::Malformed);
    }
    let (message, sig) = body.split_at(body.len() - SIG_LEN);
    let mut signature = [0u8; SIG_LEN];
    signature.copy_from_slice(sig);
    Ok(Parts {
        message: message.to_vec(),
        signature,
        footer,
    })
}

impl Parts {
    /// Verify the signature over the header, message, footer and the
    /// implicit assertion `implicit` under `key`.
    pub fn verify(&self, key: &VerifyingKey, implicit: &[u8]) -> Result<(), Refusal> {
        let m2 = pae(&[HEADER.as_bytes(), &self.message, &self.footer, implicit]);
        let sig = Signature::from_bytes(&self.signature);
        key.verify_strict(&m2, &sig).map_err(|_| Refusal::Signature)
    }
}

/// [`split`] and [`Parts::verify`] with an EXPECTED footer (the
/// specification's verify: a footer other than `footer` is refused,
/// compared in constant time): the message.
pub fn verify(
    key: &VerifyingKey,
    token: &str,
    footer: &[u8],
    implicit: &[u8],
) -> Result<Vec<u8>, Refusal> {
    let parts = split(token)?;
    if !ct_eq(&parts.footer, footer) {
        return Err(Refusal::Malformed);
    }
    parts.verify(key, implicit)?;
    Ok(parts.message)
}

/// Constant-time equality of two byte strings (the length is public).
fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    use subtle::ConstantTimeEq as _;
    a.ct_eq(b).into()
}

#[cfg(test)]
mod tests;
