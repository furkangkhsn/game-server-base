//! Every validation rule, one refusal each — and the rule's edge on
//! both sides where it has one (the skew, the expiry, the lifetime).

use super::*;
use crate::issuer::{Issuer, footer};
use crate::keys::IssuerKey;

const NOW: i64 = 1_800_000_000;
const AUD: &str = "eu-1";

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct Loadout {
    character: u64,
    class: String,
}

fn issuer(kid: &str, seed: u8) -> Issuer {
    Issuer::new(IssuerKey::from_seed(kid, [seed; 32]).expect("valid"))
}

fn claims() -> Claims<Loadout> {
    let game = Loadout {
        character: 9001,
        class: "mage".into(),
    };
    Claims::new("ann", 7, AUD, NOW, 300, game).expect("entropy")
}

fn validator(keys: &[&Issuer]) -> Validator<Loadout> {
    Validator::new(AUD, keys.iter().map(|i| i.trusted())).expect("valid")
}

fn refusal(r: Result<Verified<Loadout>, TicketError>) -> TicketError {
    r.expect_err("refused")
}

fn refused(reason: TicketReason) -> TicketError {
    TicketError::Refused(reason)
}

#[test]
fn a_good_ticket_yields_identity_room_and_the_signed_game_claims() {
    let lobby = issuer("lobby-1", 1);
    let token = lobby.mint(&claims()).expect("mint");
    let v = validator(&[&lobby]).verify_at(token.as_bytes(), NOW + 1);
    let v = v.expect("accepted");
    assert_eq!(v.claims, claims_with_id(&v.claims.ticket_id));
    let t = v.ticket();
    assert_eq!((t.player.as_str(), t.room.0), ("ann", 7));
    let extra = t.extra.expect("the game's claims");
    assert_eq!(&extra[..], br#"{"character":9001,"class":"mage"}"#);
}

fn claims_with_id(id: &str) -> Claims<Loadout> {
    Claims {
        ticket_id: id.to_owned(),
        ..claims()
    }
}

#[test]
fn the_signature_is_checked() {
    let lobby = issuer("lobby-1", 1);
    let forger = issuer("lobby-1", 2); // the same key id, another key
    let forged = forger.mint(&claims()).expect("mint");
    let v = validator(&[&lobby]);
    assert_eq!(
        refusal(v.verify_at(forged.as_bytes(), NOW)),
        refused(TicketReason::Signature)
    );
    // The genuine signature over other claims (room 8 instead of 7).
    use base64::Engine as _;
    let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD;
    let genuine = lobby.mint(&claims()).expect("mint");
    let (body, foot) = genuine["v4.public.".len()..]
        .split_once('.')
        .expect("footer");
    let body = b64.decode(body).expect("b64");
    let sig = &body[body.len() - 64..];
    let other = Claims {
        room: 8,
        ..claims()
    };
    let mut forged = crate::claims::encode(&other).expect("json");
    forged.extend_from_slice(sig);
    let spliced = format!("v4.public.{}.{foot}", b64.encode(forged));
    assert_eq!(
        refusal(v.verify_at(spliced.as_bytes(), NOW)),
        refused(TicketReason::Signature)
    );
}

#[test]
fn only_a_trusted_key_id_is_looked_up_and_rotation_works() {
    let (old, new) = (issuer("k-old", 1), issuer("k-new", 2));
    let both = validator(&[&old, &new]);
    for who in [&old, &new] {
        let t = who.mint(&claims()).expect("mint");
        assert!(
            both.verify_at(t.as_bytes(), NOW).is_ok(),
            "{}",
            who.trusted().kid()
        );
    }
    let retired = validator(&[&new]);
    let t = old.mint(&claims()).expect("mint");
    assert_eq!(
        refusal(retired.verify_at(t.as_bytes(), NOW)),
        refused(TicketReason::UnknownKey)
    );
    assert!(Validator::<Loadout>::new(AUD, [old.trusted(), old.trusted()]).is_err());
}

#[test]
fn what_is_not_a_v4_public_token_with_a_kid_is_malformed() {
    let lobby = issuer("lobby-1", 1);
    let v = validator(&[&lobby]);
    let key = IssuerKey::from_seed("lobby-1", [1; 32]).expect("valid");
    let msg = crate::claims::encode(&claims()).expect("json");
    let no_footer = paseto::sign(&key.key, &msg, b"", b"");
    let no_kid = paseto::sign(&key.key, &msg, br#"{"key":"lobby-1"}"#, b"");
    let long = "v4.public.".to_string() + &"A".repeat(MAX_TOKEN_BYTES);
    for bad in [
        &b"\xff\xfe"[..],
        long.as_bytes(),
        b"v3.public.AAAA",
        no_footer.as_bytes(),
        no_kid.as_bytes(),
    ] {
        assert_eq!(
            refusal(v.verify_at(bad, NOW)),
            refused(TicketReason::Malformed)
        );
    }
}

/// A token whose message is `json`, signed by `lobby-1` (seed 1).
fn signed(json: &str) -> String {
    let key = IssuerKey::from_seed("lobby-1", [1; 32]).expect("valid");
    paseto::sign(&key.key, json.as_bytes(), &footer("lobby-1"), b"")
}

#[test]
fn signed_but_ill_formed_claims_are_refused_as_claims() {
    let v = validator(&[&issuer("lobby-1", 1)]);
    let base = r#""room":7,"aud":"eu-1","iat":"2027-01-15T08:00:00Z","exp":"2027-01-15T08:05:00Z","jti":"j1""#;
    let ext = r#""ext":{"character":1,"class":"rogue"}"#;
    for json in [
        format!("{{{base},{ext}}}"),                       // no sub
        format!(r#"{{"sub":"",{base},{ext}}}"#),           // empty sub
        format!(r#"{{"sub":"ann",{base}}}"#),               // no game claims
        format!(r#"{{"sub":"ann",{base},"ext":{{"class":3}}}}"#), // wrong type
        format!(r#"{{"sub":"ann",{base},{ext},"sub":"bob"}}"#), // duplicated
        r#"{"sub":"ann","room":7,"aud":"eu-1","iat":"yesterday","exp":"2027-01-15T08:05:00Z","jti":"j","ext":{"character":1,"class":"r"}}"#.to_string(),
        "not json".to_string(),
    ] {
        assert_eq!(
            refusal(v.verify_at(signed(&json).as_bytes(), 1_800_000_000)),
            refused(TicketReason::Claims),
            "{json}"
        );
    }
    let good = format!(r#"{{"sub":"ann",{base},{ext},"lobby":"extra claims are ignored"}}"#);
    assert!(v.verify_at(signed(&good).as_bytes(), 1_800_000_000).is_ok());
}

#[test]
fn the_audience_is_checked() {
    let lobby = issuer("lobby-1", 1);
    let elsewhere = Claims {
        audience: "us-2".into(),
        ..claims()
    };
    let t = lobby.mint(&elsewhere).expect("mint");
    assert_eq!(
        refusal(validator(&[&lobby]).verify_at(t.as_bytes(), NOW)),
        refused(TicketReason::Audience)
    );
}

#[test]
fn expiry_and_issue_time_hold_with_the_skew_on_the_right_side() {
    let lobby = issuer("lobby-1", 1);
    let v = validator(&[&lobby]).with_skew(30);
    let t = lobby.mint(&claims()).expect("mint"); // iat NOW, exp NOW+300
    let at = |now| v.verify_at(t.as_bytes(), now).err();
    assert_eq!(at(NOW + 300 + 29), None, "inside the skew after expiry");
    assert_eq!(at(NOW + 300 + 30), Some(refused(TicketReason::Expired)));
    assert_eq!(at(NOW - 30), None, "a client clock 30 s behind");
    assert_eq!(at(NOW - 31), Some(refused(TicketReason::NotYetValid)));
    let later = Claims {
        not_before: Some(NOW + 100),
        ..claims()
    };
    let t = lobby.mint(&later).expect("mint");
    assert_eq!(v.verify_at(t.as_bytes(), NOW + 70).err(), None);
    assert_eq!(
        v.verify_at(t.as_bytes(), NOW + 69).err(),
        Some(refused(TicketReason::NotYetValid))
    );
}

#[test]
fn a_ticket_living_longer_than_allowed_is_refused() {
    let lobby = issuer("lobby-1", 1);
    let v = validator(&[&lobby]).with_max_lifetime(300);
    let ok = lobby.mint(&claims()).expect("mint"); // exactly 300 s
    assert!(v.verify_at(ok.as_bytes(), NOW).is_ok());
    let long = Claims {
        expires_at: NOW + 301,
        ..claims()
    };
    let t = lobby.mint(&long).expect("mint");
    assert_eq!(
        refusal(v.verify_at(t.as_bytes(), NOW)),
        refused(TicketReason::Lifetime)
    );
}

#[test]
fn the_games_check_refuses_under_its_own_name() {
    const NO_MAGES: GameReason = GameReason::new("mages_closed");
    let lobby = issuer("lobby-1", 1);
    let v = validator(&[&lobby]).with_check(|c: &Claims<Loadout>| {
        if c.game.class == "mage" {
            Err(NO_MAGES)
        } else {
            Ok(())
        }
    });
    let t = lobby.mint(&claims()).expect("mint");
    assert_eq!(
        refusal(v.verify_at(t.as_bytes(), NOW)),
        TicketError::Game(NO_MAGES)
    );
}

#[tokio::test]
async fn reusable_by_default_single_use_when_asked() {
    let lobby = issuer("lobby-1", 1);
    let t = lobby.mint(&claims()).expect("mint");
    let reusable = validator(&[&lobby]);
    for _ in 0..2 {
        assert!(reusable.validate_at(t.as_bytes(), NOW).await.is_ok());
    }
    let once = validator(&[&lobby]).single_use(ReplayGuard::spawn(16));
    assert!(once.validate_at(t.as_bytes(), NOW).await.is_ok());
    let again = once.validate_at(t.as_bytes(), NOW + 1).await;
    assert_eq!(again.err(), Some(refused(TicketReason::Replayed)));
    // A refused ticket is never consumed: an expired one, then good.
    let other = lobby.mint(&claims()).expect("mint");
    let late = once.validate_at(other.as_bytes(), NOW + 999).await;
    assert_eq!(late.err(), Some(refused(TicketReason::Expired)));
    assert!(once.validate_at(other.as_bytes(), NOW).await.is_ok());
}

#[tokio::test]
async fn the_hook_reports_through_the_engines_shape() {
    let lobby = issuer("lobby-1", 1);
    let now = crate::time::now();
    let fresh = Claims::new(
        "ann",
        7,
        AUD,
        now,
        60,
        Loadout {
            character: 1,
            class: "rogue".into(),
        },
    );
    let t = lobby.mint(&fresh.expect("entropy")).expect("mint");
    let auth = validator(&[&lobby]).into_auth(Duration::from_secs(1));
    let ok = (auth.validator)(bytes::Bytes::from(t))
        .await
        .expect("valid");
    assert_eq!((ok.player.as_str(), ok.room.0), ("ann", 7));
    let bad = (auth.validator)(bytes::Bytes::from_static(b"nope")).await;
    assert_eq!(bad.err(), Some(refused(TicketReason::Malformed)));
}
