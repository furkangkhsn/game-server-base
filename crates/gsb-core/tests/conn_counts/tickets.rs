//! Every AUTH a ticket-auth server decides is counted (B21): accepted,
//! or refused under exactly one `TicketReason` — the validator's named
//! reason, the game's own check (under its name besides), a free-text
//! rejection, the hook's timeout, a validator that died, no ticket at
//! all. The counts ride the connection's samples like every other delta.

use std::future::Future;
use std::pin::Pin;

use gsb_core::auth::{GameReason, TicketError, TicketReason, ValidatedTicket};
use gsb_core::id::RoomId;

use super::*;
use rig::Conn;

const SEASON: GameReason = GameReason::new("season_pass");

/// A validator answering by the ticket's text.
fn hook() -> TicketAuth {
    type Validation = Pin<Box<dyn Future<Output = Result<ValidatedTicket, TicketError>> + Send>>;
    let validator = Arc::new(|t: ::bytes::Bytes| -> Validation {
        Box::pin(async move {
            match t.as_ref() {
                b"ok" => Ok(ValidatedTicket::new("ann", RoomId(1))),
                b"expired" => Err(TicketError::Refused(TicketReason::Expired)),
                b"season" => Err(TicketError::Game(SEASON)),
                b"free" => Err(TicketError::Rejected("no such ticket".into())),
                b"slow" => std::future::pending().await,
                _ => panic!("the validator dies"),
            }
        })
    });
    TicketAuth {
        validator,
        timeout: Duration::from_millis(100),
    }
}

/// AUTH presenting `ticket`; the reply's op.
async fn present(c: &mut Conn, ticket: &[u8]) -> u16 {
    let auth = base::Auth {
        name: String::new(),
        ticket: ticket.to_vec(),
        protocol_version: 0,
    };
    c.send(op::base::AUTH_REQ, auth.encode_to_vec()).await;
    let frames = c.frames_until(op::base::ERROR).await;
    frames.last().expect("a reply").op
}

#[tokio::test]
async fn every_ticket_decision_is_counted_under_its_reason() {
    // Three attempts per connection (the AUTH attempt window's ordinary
    // path), so the refusals take two connections.
    let mut a = Conn::open_ticketed(hook());
    for t in [&b"expired"[..], b"season", b"free"] {
        assert_eq!(present(&mut a, t).await, op::base::ERROR);
    }
    let a = a.close().await.tickets;
    let mut b = Conn::open_ticketed(hook());
    for t in [&b"slow"[..], b"boom", b""] {
        assert_eq!(present(&mut b, t).await, op::base::ERROR);
    }
    let b = b.close().await.tickets;
    let mut c = Conn::open_ticketed(hook());
    let auth = base::Auth {
        name: String::new(),
        ticket: b"ok".to_vec(),
        protocol_version: 0,
    };
    c.send(op::base::AUTH_REQ, auth.encode_to_vec()).await;
    c.until(op::base::AUTH_RESULT).await;
    let c = c.close().await.tickets;

    let mut all = a;
    all.add_all(&b);
    all.add_all(&c);
    assert_eq!(all.accepted(), 1);
    for (reason, want) in [
        (TicketReason::Expired, 1),
        (TicketReason::Game, 1),
        (TicketReason::Other, 1),
        (TicketReason::TimedOut, 1),
        (TicketReason::ValidatorLost, 1),
        (TicketReason::Missing, 1),
    ] {
        assert_eq!(all.rejected(reason), want, "{reason:?}");
    }
    assert_eq!(all.rejected_total(), 6, "one reason per refusal");
    assert_eq!(all.game_slots(), &[(SEASON, 1)]);
}

/// The counts are deltas: a refusal flushed in an earlier sample is not
/// carried again by a later one (the actor's flush interval is 500 ms;
/// the pause lets it flush between the two AUTHs).
#[tokio::test]
async fn a_flushed_refusal_is_not_counted_twice() {
    let mut c = Conn::open_ticketed(hook());
    assert_eq!(present(&mut c, b"expired").await, op::base::ERROR);
    tokio::time::sleep(Duration::from_millis(600)).await;
    assert_eq!(present(&mut c, b"free").await, op::base::ERROR);
    let samples = {
        c.tell(gsb_core::conn::ConnIn::Closed {
            reason: "test over".into(),
        })
        .await;
        c.actor_done().await;
        c.take_samples()
    };
    assert!(samples.len() >= 2, "the actor flushed between the AUTHs");
    let mut all = gsb_core::metrics::TicketCounts::default();
    for s in &samples {
        all.add_all(&s.tickets);
    }
    assert_eq!(all.rejected(TicketReason::Expired), 1);
    assert_eq!(all.rejected(TicketReason::Other), 1);
}
