//! Signed tickets for gsb (BACKLOG B21) — an OPT-IN building block.
//!
//! The engine authenticates by delegation: the platform (a lobby,
//! matchmaking) hands each client a ticket, and the game server checks
//! it through ONE seam, [`gsb_core::auth::TicketAuth`]. The engine is
//! format-agnostic; a game may plug its own validator (its auth service,
//! Steam, …) and never touch this crate. This crate is one ready answer:
//!
//! - **Format:** PASETO v4.public — Ed25519 signatures, JSON claims, a
//!   standard a platform in another language mints with its own library
//!   ([`paseto`], checked against the official test vectors).
//! - **Claims:** [`Claims<T>`] — the standard ones the crate checks
//!   (player, room, audience, issued-at / not-before / expiry with clock
//!   skew, a unique id, the issuer key's id) and the GAME's own `T`, typed
//!   by the game and handed to its join hooks as verified bytes.
//! - **[`Issuer`]** mints; **[`Validator<T>`]** verifies, runs the game's
//!   own stateless check ([`Validator::with_check`]), optionally refuses
//!   a replay ([`ReplayGuard`]), and becomes a `TicketAuth` in one line
//!   ([`Validator::into_auth`]). Every refusal is counted by the engine
//!   under its reason (`gsb_net_tickets_rejected_total{reason}`).
//! - **[`JoinGrant`]:** the lobby's answer to a client — doors, the rUDP
//!   server key to pin, the ticket, the room.
//! - **[`ValidatorConfig`]:** the same validator from a config table.
//!
//! ```
//! use gsb_ticket::{Claims, Issuer, IssuerKey, Validator};
//!
//! #[derive(serde::Serialize, serde::Deserialize, Debug, PartialEq)]
//! struct Loadout { class: String }
//!
//! let issuer = Issuer::new(IssuerKey::generate("lobby-1").unwrap());
//! let now = 1_800_000_000;
//! let claims = Claims::new("ann", 1, "eu-1", now, 300, Loadout { class: "mage".into() }).unwrap();
//! let token = issuer.mint(&claims).unwrap();
//!
//! let validator = Validator::<Loadout>::new("eu-1", [issuer.trusted()]).unwrap();
//! let verified = validator.verify_at(token.as_bytes(), now + 10).unwrap();
//! assert_eq!(verified.claims.game.class, "mage");
//! assert_eq!(verified.ticket().player, "ann");
//! ```
//!
//! Pure Rust (ed25519-dalek over the workspace's curve25519-dalek, serde_json,
//! base64): no ring, no aws-lc, no C; no `unsafe`, no locks.

pub mod claims;
pub mod config;
pub mod grant;
pub mod issuer;
pub mod keys;
pub mod paseto;
pub mod replay;
pub mod time;
pub mod validator;

pub use claims::Claims;
pub use config::ValidatorConfig;
pub use grant::{Door, JoinGrant, Transport};
pub use issuer::{Issuer, MintError};
pub use keys::{IssuerKey, KeyError, TrustedKey};
pub use replay::ReplayGuard;
pub use validator::{Validator, Verified};
