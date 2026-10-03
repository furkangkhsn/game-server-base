//! The B21 showcase, end to end (docs/TICKETS.md):
//!
//! ```text
//! client ──login──▶ lobby ──grant {doors, udp key, ticket, room}──▶ client
//! client ──rUDP handshake (key pinned from the grant)──▶ game server
//! client ──AUTH {ticket}──▶ validator: signature, claims, the game's check
//! client ──JOIN room──▶ the game spawns the character from the VERIFIED claims
//! ```
//!
//! - [`lobby`]: the platform side — accounts, characters, the signed
//!   grant (a plain function standing in for an HTTP endpoint).
//! - [`server`]: the game server — sealed rUDP + TCP doors, the ticket
//!   hook with the game's check.
//! - [`game`]: the game — the 2D demo, spawning each character where its
//!   class stands, from the ticket's claims.
//! - [`client`]: log in, connect from the grant, join, see the world.

pub mod client;
pub mod game;
pub mod lobby;
pub mod server;
