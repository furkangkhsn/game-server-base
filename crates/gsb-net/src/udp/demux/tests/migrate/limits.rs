//! Migration's limits on a demux driven directly: the amplification
//! budget, a writer that cannot be told, a validation cut off by its
//! session's end, and the tagged kinds no client sends. Child of
//! `migrate`, so its rig is shared.

use super::*;
use crate::udp::path::{PathProbe, RESPONSE_LEN};

/// The amplification budget at the demux: a probe that has heard 2 B
/// may not be sent a 9 B challenge (withheld, counted); one more byte
/// heard and it may.
#[tokio::test]
async fn the_challenge_never_exceeds_three_times_what_the_candidate_sent() {
    let mut r = rig(4).await;
    let b = addr(&r.b);
    let now = Instant::now();
    r.d.sessions.get_mut(r.key).unwrap().path = Some(PathProbe::new(b, 5, 2, now));
    r.d.challenge(r.key, now);
    assert_eq!(r.d.mig.amplification_capped, 1);
    assert_eq!(got(&r.b).await, None);
    r.d.sessions
        .get_mut(r.key)
        .unwrap()
        .path
        .as_mut()
        .unwrap()
        .heard(1);
    r.d.challenge(r.key, now);
    assert_eq!(nonce_of(&got(&r.b).await.unwrap()), 5);
}

/// A writer whose channel is full cannot be told: no move (counted),
/// the validation stays pending, and the next response moves it.
#[tokio::test]
async fn a_full_writer_channel_defers_the_move() {
    let mut r = rig(1).await;
    let b = addr(&r.b);
    feed(&mut r.d, b, &raw(b"x"));
    let nonce = nonce_of(&got(&r.b).await.unwrap());
    let key = r.key;
    let out = r.d.sessions.get(key).unwrap().out_tx.clone();
    out.try_send(vec![]).unwrap(); // the writer is behind
    let resp = encode_path_response(CID, nonce);
    assert_eq!(resp.len(), RESPONSE_LEN);
    feed(&mut r.d, b, &resp);
    assert_eq!(r.d.mig.changes_not_forwarded, 1);
    assert_eq!(r.d.sessions.key_at(&b), None, "not moved");
    r.outbox.try_recv().unwrap();
    feed(&mut r.d, b, &resp);
    assert_eq!(r.d.sessions.key_at(&b), Some(key));
}

/// A session that ends with a validation pending books it: in time, as
/// open at the end; past its time, as timed out.
#[tokio::test]
async fn a_pending_validation_ends_with_its_session() {
    for (late, open, timed_out) in [(false, 1, 0), (true, 0, 1)] {
        let mut r = rig(4).await;
        let b = addr(&r.b);
        let at = Instant::now()
            - if late {
                VALIDATION_TIMEOUT
            } else {
                Duration::ZERO
            };
        r.d.sessions.get_mut(r.key).unwrap().path = Some(PathProbe::new(b, 5, 9, at));
        r.d.remove_session(r.key);
        let m = r.d.mig;
        assert_eq!(
            (m.validations_open_at_end, m.validations_timed_out),
            (open, timed_out)
        );
    }
}

/// Tagged kinds a client never sends: a FRAG is refused by rule, a
/// HELLO is malformed; neither makes a candidate.
#[tokio::test]
async fn tagged_kinds_a_client_never_sends_make_no_candidate() {
    let mut r = rig(4).await;
    let b = addr(&r.b);
    feed(&mut r.d, b, &tag(CID, &[KIND_FRAG, 0, 0, 0, 1, 1, 1]));
    feed(&mut r.d, b, &tag(CID, &encode_hello(1, 2)));
    feed(&mut r.d, b, &[KIND_RAW | KIND_CID_TAG, 1, 2]);
    assert_eq!((r.d.frag_refused, r.d.bad_datagrams), (1, 2));
    assert_eq!(r.d.mig.validations_started, 0);
}

/// A move to another IP is a migration, not a port-only one (the writer
/// then resets the path estimate).
#[tokio::test]
async fn a_new_ip_is_not_a_port_only_move() {
    let mut r = rig(4).await;
    let far = UdpSocket::bind("127.0.0.2:0").await.expect("loopback alias");
    let to = addr(&far);
    feed(&mut r.d, to, &raw(b"x"));
    let nonce = nonce_of(&got(&far).await.unwrap());
    feed(&mut r.d, to, &encode_path_response(CID, nonce));
    assert_eq!(r.d.sessions.key_at(&to), Some(r.key));
    assert_eq!((r.d.mig.migrations, r.d.mig.migrations_port_only), (1, 0));
}
