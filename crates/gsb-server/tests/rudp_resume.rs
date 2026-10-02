//! Resume over rUDP, end to end (BACKLOG B7): the regression net under
//! the coming rUDP rounds — connection migration (B3: a connection id
//! independent of the 4-tuple) and encryption (B5). Today the demux
//! keys a session by the peer address, so a client whose address
//! changes (a NAT rebinding, a mobile handover) or whose socket died
//! comes back as a NEW session: a new socket, a new cookie handshake,
//! and a resume under the same credentials (RECONNECT §5). These tests
//! lock that fallback through the real server, plaintext:
//!
//! - a vanished client (no LEAVE; rUDP has no FIN, so the server learns
//!   it from the idle sweep) is parked, and a client from a new local
//!   port resumes the SAME entity — on one room actor and on the
//!   sharded grid, whose resume is a broadcast (RECONNECT §6);
//! - a new session that arrives while the old one is still live takes
//!   it over (F32): the old one gets `ERROR` 9 (`superseded`), the
//!   membership and the entity move to the new one;
//! - past the grace, the demo's bot holds the entity and the returning
//!   player reclaims it; with no grace, the same credentials make a
//!   transparent fresh join.
//!
//! Each flow runs on TCP beside rUDP, one function per scenario with
//! per-door expectations; B3 added its own next to these: with
//! `udp_migration` on, a client whose address changes keeps its session
//! (no new handshake, no resume, no close), and the fallback above still
//! holds on such a door. Real sockets and the
//! rUDP client's `Instant`-clocked retransmit rule out the paused
//! clock; every wait is a condition with a hang guard (CONTRIBUTING
//! "Gerçek saatli testler"), every count is read from the server's own
//! reports and pinned exactly.

#![cfg(feature = "game-demo")]

#[path = "rudp_resume/flows.rs"]
mod flows;
#[path = "rudp_resume/grace.rs"]
mod grace;
#[path = "rudp_resume/player.rs"]
mod player;
#[path = "rudp_resume/rig.rs"]
mod rig;

use rig::{Door, Shape};

#[tokio::test]
async fn a_vanished_rudp_client_resumes_from_a_new_port() {
    flows::vanish_then_resume(Door::Udp, Shape::Single).await;
}

#[tokio::test]
async fn a_vanished_tcp_client_resumes_from_a_new_connection() {
    flows::vanish_then_resume(Door::Tcp, Shape::Single).await;
}

#[tokio::test]
async fn a_vanished_rudp_client_resumes_on_the_sharded_grid() {
    flows::vanish_then_resume(Door::Udp, Shape::Sharded).await;
}

#[tokio::test]
async fn a_live_rudp_session_is_taken_over_by_the_newer_one() {
    flows::takeover(Door::Udp).await;
}

#[tokio::test]
async fn a_live_tcp_session_is_taken_over_by_the_newer_one() {
    flows::takeover(Door::Tcp).await;
}

#[tokio::test]
async fn past_the_grace_the_rudp_client_reclaims_its_entity_from_the_bot() {
    grace::resume_after_the_grace(Door::Udp).await;
}

#[tokio::test]
async fn without_a_grace_the_rudp_client_joins_fresh() {
    grace::no_grace_is_a_fresh_join(Door::Udp).await;
}

// B3: connection migration on. The migrated session needs no handshake
// and no resume; the fallback (a vanished client resumes from a new
// socket) still holds beside it.

#[tokio::test]
async fn a_migrating_rudp_session_survives_its_address_change() {
    flows::migrate(Door::Migrating).await;
}

#[tokio::test]
async fn with_migration_on_a_vanished_rudp_client_still_resumes() {
    flows::vanish_then_resume(Door::Migrating, Shape::Single).await;
}
