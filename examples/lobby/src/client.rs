//! The client: log in at the lobby, connect from the grant (the rUDP door,
//! the server key pinned from the grant), join with the ticket, and read
//! the room until its own character shows up in a snapshot.

use std::time::{Duration, Instant};

use gsb_client::grant;
use gsb_client::{ClientError, Conn, Recv};
use gsb_demo::game::{Private, WorldSnapshot, private};
use gsb_demo::op;
use gsb_ticket::{JoinGrant, Transport};
use prost::Message;

/// How long each step may take.
pub const WINDOW: Duration = Duration::from_secs(5);

/// A joined session, and where its character stands in the world.
pub struct Played {
    pub conn: Conn,
    /// The identity the server took from the ticket.
    pub player: String,
    /// The character's wire id and its position in the first snapshot
    /// that carried it.
    pub entity: u64,
    pub at: (i32, i32),
}

/// Connect from `grant_json` over `transport`, join, and wait for the
/// character in a snapshot.
pub async fn play(grant_json: &str, transport: Transport) -> Result<Played, ClientError> {
    let grant =
        JoinGrant::from_json(grant_json).map_err(|e| ClientError::Io(std::io::Error::other(e)))?;
    let mut conn = grant::connect(&grant, transport, &[]).await?;
    let mut early = Vec::new();
    let joined = grant::join(&mut conn, &grant, "client", WINDOW, |f| early.push(f)).await?;
    let entity = joined.entity;
    let mut found = early
        .iter()
        .find_map(|f| position(f.op, &f.payload, entity));
    let deadline = Instant::now() + WINDOW;
    while found.is_none() {
        let left = deadline
            .checked_duration_since(Instant::now())
            .ok_or(ClientError::TimedOut)?;
        match conn.recv(left).await? {
            Recv::Frame(f) => found = position(f.op, &f.payload, entity),
            Recv::Closed => return Err(ClientError::Closed),
            Recv::Quiet => {}
        }
    }
    Ok(Played {
        conn,
        player: joined.auth.player,
        entity,
        at: found.unwrap_or_default(),
    })
}

/// `entity`'s position in a snapshot frame (the room's, or the one-shot
/// full of a private frame), if it carries the entity.
fn position(code: u16, payload: &[u8], entity: u64) -> Option<(i32, i32)> {
    let snapshot = match code {
        op::WORLD_SNAPSHOT => WorldSnapshot::decode(payload).ok()?,
        op::PRIVATE => match Private::decode(payload).ok()?.payload? {
            private::Payload::Snapshot(s) => s,
            _ => return None,
        },
        _ => return None,
    };
    let r = snapshot.entities.iter().find(|r| r.entity == entity)?;
    Some((r.x, r.y))
}
