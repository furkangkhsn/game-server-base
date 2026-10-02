//! Who is joining: the authenticated identity and the game's verified
//! claims, as the join hooks see them.

use bytes::Bytes;

/// A fresh join as the game's hooks receive it
/// ([`crate::room::GameLogic::on_join_verified`], the sharded room's
/// [`crate::registry::HomeRoute`]).
///
/// `#[non_exhaustive]`: the engine builds it, a game reads it; a later
/// field is additive. Tests build one with [`Self::new`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct Joiner<'a> {
    /// The resume key: the ticket's validated player, the client-claimed
    /// `Auth.name` on the local-auth path (development only — anyone can
    /// claim any name there), empty for an anonymous session.
    pub identity: &'a str,
    /// The game's own claims, verified by the ticket validator
    /// ([`super::ValidatedTicket::extra`]) — what a game reads instead of
    /// trusting the client ("this player comes with this loadout").
    /// `None` on the local-auth path, for a validator that verifies no
    /// extra claims, and for an anonymous session.
    pub claims: Option<&'a Bytes>,
}

impl<'a> Joiner<'a> {
    /// A joiner with `identity` and no claims.
    pub const fn new(identity: &'a str) -> Self {
        Self {
            identity,
            claims: None,
        }
    }

    /// The same joiner carrying `claims`.
    pub const fn with_claims(mut self, claims: Option<&'a Bytes>) -> Self {
        self.claims = claims;
        self
    }
}
