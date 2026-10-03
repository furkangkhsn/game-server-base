//! The validator side: verify a ticket and hand the engine its identity
//! — a [`gsb_core::auth::TicketAuth`] in one line ([`Validator::into_auth`]).
//!
//! The order of the checks, each refusal under its own reason:
//!
//! 1. size, `v4.public.` header, base64url, footer `{"kid":…}` → `malformed`
//! 2. the key id → a trusted key, or `unknown_key`
//! 3. the Ed25519 signature (`verify_strict`) → `signature`
//! 4. the claims' shape and types (the game's `T` too) → `claims`
//! 5. `aud` → `audience`
//! 6. `nbf` / `iat` in the future beyond the skew → `not_yet_valid`;
//!    `exp` passed beyond the skew → `expired`; `exp - iat` above the
//!    allowed lifetime → `lifetime`
//! 7. the game's own check → `game` (+ its name)
//! 8. single-use only: an id seen before → `replayed`, a guard that
//!    cannot answer → `replay_unavailable`
//!
//! 1–7 are stateless (the ticket alone answers them); 8 asks the guard's
//! actor last, so a ticket refused for anything else is never consumed.
//! What needs game STATE (the roster, a full team, a kick from this room)
//! is the room's join hook's, on the tick — not the validator's.

use std::sync::Arc;
use std::time::Duration;

use gsb_core::auth::{GameReason, TicketAuth, TicketError, TicketReason, ValidatedTicket};
use gsb_core::id::RoomId;
use serde::de::DeserializeOwned;

use crate::claims::{self, Claims};
use crate::keys::{KeyError, TrustedKey};
use crate::paseto;
use crate::replay::ReplayGuard;
use crate::time;

/// The default clock-skew allowance, seconds.
pub const DEFAULT_SKEW_SECS: i64 = 30;
/// The default longest lifetime a ticket may declare, seconds.
pub const DEFAULT_MAX_LIFETIME_SECS: i64 = 900;
/// The longest token read, in bytes.
pub const MAX_TOKEN_BYTES: usize = 4096;
/// The longest footer read, in bytes.
const MAX_FOOTER_BYTES: usize = 256;

/// One validation, as the engine's hook returns it.
type Validation = std::pin::Pin<
    Box<dyn std::future::Future<Output = Result<ValidatedTicket, TicketError>> + Send>,
>;

/// The game's own check (see [`Validator::with_check`]).
type Check<T> = Arc<dyn Fn(&Claims<T>) -> Result<(), GameReason> + Send + Sync>;

/// A verified ticket: its claims, and the game's part as the exact JSON
/// the issuer signed (what [`ValidatedTicket::extra`] carries).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Verified<T> {
    pub claims: Claims<T>,
    pub extra: Option<bytes::Bytes>,
}

impl<T> Verified<T> {
    /// What the engine needs: the identity, the pinned room, the claims.
    pub fn ticket(&self) -> ValidatedTicket {
        ValidatedTicket {
            player: self.claims.player.clone(),
            room: RoomId(self.claims.room),
            extra: self.extra.clone(),
        }
    }
}

/// Verifies tickets for one audience against a set of trusted issuer
/// keys. Reusable within its expiry by default (a reconnect re-presents
/// the same ticket — RECONNECT §5); [`Self::single_use`] opts in to the
/// replay guard.
pub struct Validator<T> {
    keys: Vec<TrustedKey>,
    audience: String,
    skew: i64,
    max_lifetime: i64,
    check: Option<Check<T>>,
    replay: Option<ReplayGuard>,
}

impl<T: DeserializeOwned> Validator<T> {
    /// A validator for `audience` trusting `keys` (at least one; key ids
    /// distinct), with the default skew and lifetime.
    pub fn new(
        audience: impl Into<String>,
        keys: impl IntoIterator<Item = TrustedKey>,
    ) -> Result<Self, KeyError> {
        let audience = audience.into();
        if audience.is_empty() {
            return Err(KeyError("the audience is empty".into()));
        }
        let keys: Vec<TrustedKey> = keys.into_iter().collect();
        if keys.is_empty() {
            return Err(KeyError("no trusted issuer key".into()));
        }
        for (i, k) in keys.iter().enumerate() {
            if keys[..i].iter().any(|o| o.kid == k.kid) {
                return Err(KeyError(format!("key id `{}` is listed twice", k.kid)));
            }
        }
        Ok(Self {
            keys,
            audience,
            skew: DEFAULT_SKEW_SECS,
            max_lifetime: DEFAULT_MAX_LIFETIME_SECS,
            check: None,
            replay: None,
        })
    }

    /// The clock-skew allowance (seconds; negative reads as 0).
    pub fn with_skew(mut self, secs: i64) -> Self {
        self.skew = secs.max(0);
        self
    }

    /// The longest lifetime (`exp - iat`) a ticket may declare, seconds.
    pub fn with_max_lifetime(mut self, secs: i64) -> Self {
        self.max_lifetime = secs.max(0);
        self
    }

    /// The game's own STATELESS check, run after the signature and the
    /// standard claims: refuse with the game's reason name ("client too
    /// old", "this mode needs the season pass", "wrong region"), counted
    /// exactly under that name.
    pub fn with_check(
        mut self,
        check: impl Fn(&Claims<T>) -> Result<(), GameReason> + Send + Sync + 'static,
    ) -> Self {
        self.check = Some(Arc::new(check));
        self
    }

    /// Single-use tickets: each ticket id is admitted once (see
    /// [`crate::replay`]); a reconnect then needs a fresh ticket.
    pub fn single_use(mut self, guard: ReplayGuard) -> Self {
        self.replay = Some(guard);
        self
    }

    /// Checks 1–7 at `now` (Unix seconds).
    pub fn verify_at(&self, token: &[u8], now: i64) -> Result<Verified<T>, TicketError> {
        let refused = |r| TicketError::Refused(r);
        if token.len() > MAX_TOKEN_BYTES {
            return Err(refused(TicketReason::Malformed));
        }
        let token = std::str::from_utf8(token).map_err(|_| refused(TicketReason::Malformed))?;
        let parts = paseto::split(token).map_err(|_| refused(TicketReason::Malformed))?;
        let kid = kid_of(&parts.footer).ok_or(refused(TicketReason::Malformed))?;
        let key = self.keys.iter().find(|k| k.kid == kid);
        let key = key.ok_or(refused(TicketReason::UnknownKey))?;
        parts
            .verify(&key.key, b"")
            .map_err(|_| refused(TicketReason::Signature))?;
        let d = claims::decode::<T>(&parts.message).ok_or(refused(TicketReason::Claims))?;
        let c = &d.claims;
        if c.audience != self.audience {
            return Err(refused(TicketReason::Audience));
        }
        let start = c.not_before.unwrap_or(c.issued_at).max(c.issued_at);
        if now.saturating_add(self.skew) < start {
            return Err(refused(TicketReason::NotYetValid));
        }
        if now >= c.expires_at.saturating_add(self.skew) {
            return Err(refused(TicketReason::Expired));
        }
        if c.expires_at.saturating_sub(c.issued_at) > self.max_lifetime {
            return Err(refused(TicketReason::Lifetime));
        }
        if let Some(check) = &self.check {
            check(c).map_err(TicketError::Game)?;
        }
        Ok(Verified {
            claims: d.claims,
            extra: d.ext,
        })
    }

    /// Every check at `now`, the replay guard's included.
    pub async fn validate_at(&self, token: &[u8], now: i64) -> Result<Verified<T>, TicketError> {
        let v = self.verify_at(token, now)?;
        if let Some(guard) = &self.replay {
            let keep_until = v.claims.expires_at.saturating_add(self.skew);
            let jti = &v.claims.ticket_id;
            guard
                .admit(jti, keep_until, now)
                .await
                .map_err(TicketError::Refused)?;
        }
        Ok(v)
    }

    /// The engine's hook: `ServerHooks { ticket: Some(validator.into_auth(timeout)) }`.
    /// Validation runs in the connection actor's worker, off the tick.
    pub fn into_auth(self, timeout: Duration) -> TicketAuth
    where
        T: Send + Sync + 'static,
    {
        let me = Arc::new(self);
        TicketAuth {
            validator: Arc::new(move |token: bytes::Bytes| -> Validation {
                let me = Arc::clone(&me);
                Box::pin(async move {
                    let v = me.validate_at(&token, time::now()).await?;
                    Ok(v.ticket())
                })
            }),
            timeout,
        }
    }
}

/// The key id from a footer `{"kid":"…"}` (`None`: absent or not one).
fn kid_of(footer: &[u8]) -> Option<String> {
    #[derive(serde::Deserialize)]
    struct Footer {
        kid: String,
    }
    if footer.is_empty() || footer.len() > MAX_FOOTER_BYTES {
        return None;
    }
    serde_json::from_slice::<Footer>(footer).ok().map(|f| f.kid)
}

#[cfg(test)]
mod tests;
