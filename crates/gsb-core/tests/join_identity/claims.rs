//! B21: the game's verified claims (`ValidatedTicket::extra`) ride the
//! identified join to the sharded room's router (`route_verified`) and
//! to the join hook (`GameLogic::on_join_verified`) of the shard it
//! routes to — and of a single room, through the resume fallback. A
//! local-auth join carries none.

use super::*;
use gsb_core::registry::route_verified;

/// Ticket `t-mage` is player `ann` with the claims `{"class":"mage"}`.
fn claims_hook() -> TicketAuth {
    type Validation = Pin<Box<dyn Future<Output = Result<ValidatedTicket, TicketError>> + Send>>;
    let validator = Arc::new(|t: bytes::Bytes| -> Validation {
        Box::pin(async move {
            match t.as_ref() {
                b"t-mage" => Ok(ValidatedTicket::new("ann", RoomId(1))
                    .with_extra(bytes::Bytes::from_static(br#"{"class":"mage"}"#))),
                _ => Err(TicketError::Rejected("unknown ticket".into())),
            }
        })
    });
    TicketAuth {
        validator,
        timeout: Duration::from_secs(2),
    }
}

/// Three shards; the router sends a mage to shard 2 (by the CLAIMS, not
/// the identity) and everyone else to shard 0, reporting what it read.
fn by_class(seen: Seen) -> RoomFactory<(), (), (), ()> {
    Arc::new(move |_id, _cfg| {
        let shards = (0..SHARDS)
            .map(|index| {
                let logic = IdLogic {
                    index,
                    serial: 0,
                    seen: seen.clone(),
                };
                (
                    (),
                    Box::new(logic)
                        as Box<dyn ShardLogic<(), GroupKey = (), State = (), Strip = ()>>,
                )
            })
            .collect();
        let routed = seen.clone();
        BuiltRoom::Sharded {
            shards,
            home_shard: route_verified(move |_conn, joiner| {
                let mage = joiner
                    .claims
                    .is_some_and(|c| c.as_ref() == br#"{"class":"mage"}"#);
                let shard = if mage { 2 } else { 0 };
                let _ = routed.send(("route", shard, joiner.identity.to_string()));
                shard
            }),
        }
    })
}

#[tokio::test]
async fn the_verified_claims_reach_the_router_and_the_home_shards_join_hook() {
    let (seen, mut rx) = mpsc::unbounded_channel();
    let reg = registry(by_class(seen)).await;
    let (entity, _a) = login(&reg, 1, Some(claims_hook()), "trinity", b"t-mage").await;
    assert_eq!(entity / SPAN, 2, "routed by the claims");
    assert_eq!(next(&mut rx).await, ("route", 2, "ann".to_string()));
    let claims = r#"{"class":"mage"}"#.to_string();
    assert_eq!(next(&mut rx).await, ("claims", 2, claims));
    assert_eq!(next(&mut rx).await, ("join", 2, "ann".to_string()));
    // Local auth: no claims anywhere.
    let (entity, _b) = login(&reg, 2, None, "bob", b"").await;
    assert_eq!(entity / SPAN, 0);
    assert_eq!(next(&mut rx).await, ("route", 0, "bob".to_string()));
    assert_eq!(next(&mut rx).await, ("join", 0, "bob".to_string()));
}

#[tokio::test]
async fn a_single_room_hands_its_join_hook_the_verified_claims() {
    let (seen, mut rx) = mpsc::unbounded_channel();
    let reg = registry(single(seen)).await;
    let (_, _a) = login(&reg, 1, Some(claims_hook()), "trinity", b"t-mage").await;
    let claims = r#"{"class":"mage"}"#.to_string();
    assert_eq!(next(&mut rx).await, ("claims", 0, claims));
    assert_eq!(next(&mut rx).await, ("join", 0, "ann".to_string()));
}
