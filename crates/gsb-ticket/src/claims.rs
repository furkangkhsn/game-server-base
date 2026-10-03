//! The claims a ticket carries: the standard ones the crate checks, and
//! the game's own (`T`), which the crate only types and hands on.
//!
//! The payload's JSON (the PASETO message):
//!
//! ```json
//! { "sub": "player-7", "room": 42, "aud": "eu-1",
//!   "iat": "2026-10-03T12:00:00Z", "exp": "2026-10-03T12:05:00Z",
//!   "jti": "7f3a…", "iss": "lobby", "nbf": "…",
//!   "ext": { "character": 9001, "class": "mage" } }
//! ```
//!
//! Required: `sub` (the player identity — `ValidatedTicket.player`, the
//! resume key, the K4 home-shard key), `room` (the pinned room), `aud`,
//! `iat`, `exp`, `jti`. Optional: `nbf`, `iss` (informational; the key id
//! names the issuer), `ext` (the game's claims; absent reads as `null`).
//! Any other claim is ignored (a lobby may carry its own).

use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::value::RawValue;

use crate::time;

/// The longest player identity and ticket id, in bytes.
pub const ID_MAX: usize = 128;

/// A ticket's claims: the standard ones and the game's own `T`.
///
/// `T` is the game's type (`#[derive(Serialize, Deserialize)]` — a
/// character id, a class, a loadout, a party, a region, MMR,
/// entitlements, a spectator role …); `()` for none.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Claims<T> {
    /// `sub`: the player identity.
    pub player: String,
    /// `room`: the room the ticket pins.
    pub room: u64,
    /// `aud`: the realm or server the ticket was minted for.
    pub audience: String,
    /// `iat`, Unix seconds.
    pub issued_at: i64,
    /// `nbf`, Unix seconds.
    pub not_before: Option<i64>,
    /// `exp`, Unix seconds.
    pub expires_at: i64,
    /// `jti`: the ticket's unique id (logs, the single-use guard).
    pub ticket_id: String,
    /// `iss`: the issuer's name (informational).
    pub issuer: Option<String>,
    /// `ext`: the game's own claims.
    pub game: T,
}

impl<T> Claims<T> {
    /// Claims for `player` in `room` for `audience`, valid from `now` for
    /// `ttl_secs` seconds, with a fresh random ticket id (`None`: the OS
    /// entropy source failed) and the game's claims `game`.
    pub fn new(
        player: impl Into<String>,
        room: u64,
        audience: impl Into<String>,
        now: i64,
        ttl_secs: i64,
        game: T,
    ) -> Option<Self> {
        let mut id = [0u8; 16];
        getrandom::fill(&mut id).ok()?;
        Some(Self {
            player: player.into(),
            room,
            audience: audience.into(),
            issued_at: now,
            not_before: None,
            expires_at: now.saturating_add(ttl_secs),
            ticket_id: id.iter().map(|b| format!("{b:02x}")).collect(),
            issuer: None,
            game,
        })
    }
}

/// The payload as signed (writing).
#[derive(Serialize)]
struct WireOut<'a, T> {
    sub: &'a str,
    room: u64,
    aud: &'a str,
    iat: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    nbf: Option<String>,
    exp: String,
    jti: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    iss: Option<&'a str>,
    ext: &'a T,
}

/// The payload as read: borrowed, the game's part left raw.
#[derive(serde::Deserialize)]
struct WireIn<'a> {
    sub: String,
    room: u64,
    aud: String,
    iat: String,
    #[serde(default)]
    nbf: Option<String>,
    exp: String,
    jti: String,
    #[serde(default)]
    iss: Option<String>,
    #[serde(borrow, default)]
    ext: Option<&'a RawValue>,
}

/// The claims as JSON bytes (the message to sign).
pub(crate) fn encode<T: Serialize>(c: &Claims<T>) -> Result<Vec<u8>, serde_json::Error> {
    serde_json::to_vec(&WireOut {
        sub: &c.player,
        room: c.room,
        aud: &c.audience,
        iat: time::format(c.issued_at),
        nbf: c.not_before.map(time::format),
        exp: time::format(c.expires_at),
        jti: &c.ticket_id,
        iss: c.issuer.as_deref(),
        ext: &c.game,
    })
}

/// Verified claims as read: the typed claims and the game's part as the
/// exact JSON bytes the issuer signed (`None` when absent or `null`).
pub(crate) struct Decoded<T> {
    pub claims: Claims<T>,
    pub ext: Option<bytes::Bytes>,
}

/// Read the claims from a VERIFIED message. `None`: a required claim is
/// missing, ill-typed or out of bounds, a time unreadable, or the game's
/// part not a `T` — all the `claims` refusal.
pub(crate) fn decode<T: DeserializeOwned>(message: &[u8]) -> Option<Decoded<T>> {
    let w: WireIn<'_> = serde_json::from_slice(message).ok()?;
    let id_ok = |s: &str| !s.is_empty() && s.len() <= ID_MAX;
    if !id_ok(&w.sub) || !id_ok(&w.jti) || w.aud.is_empty() {
        return None;
    }
    let raw = w.ext.map(RawValue::get).filter(|r| *r != "null");
    let game: T = serde_json::from_str(raw.unwrap_or("null")).ok()?;
    let not_before = match &w.nbf {
        Some(s) => Some(time::parse(s)?),
        None => None,
    };
    Some(Decoded {
        claims: Claims {
            player: w.sub,
            room: w.room,
            audience: w.aud,
            issued_at: time::parse(&w.iat)?,
            not_before,
            expires_at: time::parse(&w.exp)?,
            ticket_id: w.jti,
            issuer: w.iss,
            game,
        },
        ext: raw.map(|r| bytes::Bytes::copy_from_slice(r.as_bytes())),
    })
}
