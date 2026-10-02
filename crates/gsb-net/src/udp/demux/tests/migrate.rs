//! Path validation and migration on a demux driven directly (BACKLOG
//! B3; module `crate::udp::path`). Three real client sockets play the
//! session's old address and two new ones, so every challenge the demux
//! sends can be read where it went — and where it did not.

use super::*;
use crate::udp::path::{VALIDATION_TIMEOUT, decode_addr};
use gsb_core::channel::{FrameBatch, Inbox};

const CID: u64 = 0x0123_4567_89AB_CDEF;

/// A migration-on demux with one session (CID [`CID`]) at socket `a`'s
/// address; sockets `b` and `c` are the session's possible new paths.
pub(super) struct Rig {
    pub(super) d: Demux,
    pub(super) a: UdpSocket,
    pub(super) b: UdpSocket,
    pub(super) c: UdpSocket,
    pub(super) key: SessionKey,
    pub(super) inbox: Inbox<ConnIn>,
    pub(super) outbox: Inbox<FrameBatch>,
}

pub(super) async fn rig(out_cap: usize) -> Rig {
    let sock = Arc::new(UdpSocket::bind("127.0.0.1:0").await.expect("bind"));
    sock.writable().await.expect("writable");
    let (mut d, _end_rx) = demux_bare(sock);
    d.migration = true;
    let a = UdpSocket::bind("127.0.0.1:0").await.expect("a");
    let b = UdpSocket::bind("127.0.0.1:0").await.expect("b");
    let c = UdpSocket::bind("127.0.0.1:0").await.expect("c");
    let (in_tx, inbox) = gsb_core::channel::channel(16);
    let (out_tx, outbox) = gsb_core::channel::channel(out_cap);
    let s = UdpSession::new(addr(&a), Some(CID), in_tx, out_tx, Instant::now());
    let key = d.sessions.insert(s);
    Rig {
        d,
        a,
        b,
        c,
        key,
        inbox,
        outbox,
    }
}

pub(super) fn addr(s: &UdpSocket) -> SocketAddr {
    s.local_addr().unwrap()
}

/// What `s` receives within 150 ms.
pub(super) async fn got(s: &UdpSocket) -> Option<Vec<u8>> {
    let mut buf = [0u8; 64];
    match tokio::time::timeout(Duration::from_millis(150), s.recv_from(&mut buf)).await {
        Ok(Ok((n, _))) => Some(buf[..n].to_vec()),
        _ => None,
    }
}

pub(super) fn raw(p: &[u8]) -> Vec<u8> {
    tag(
        CID,
        &encode_raw(&FrameBody::new(1000, Bytes::copy_from_slice(p))),
    )
}

/// The nonce of the challenge `d` (a PATH_CHALLENGE, checked).
pub(super) fn nonce_of(d: &[u8]) -> u64 {
    assert_eq!((d.len(), d[0]), (9, KIND_PATH_CHALLENGE), "{d:?}");
    u64_at(d, 1).unwrap()
}

impl Rig {
    pub(super) fn frame(&mut self) -> Option<Vec<u8>> {
        match self.inbox.try_recv() {
            Ok(ConnIn::Frame(f)) => Some(f.payload.to_vec()),
            _ => None,
        }
    }
}

/// A tagged datagram from the session's own address is the session's,
/// exactly as untagged: forwarded, nothing challenged.
#[tokio::test]
async fn a_tagged_datagram_from_the_session_s_address_is_routed() {
    let mut r = rig(4).await;
    let a = addr(&r.a);
    feed(&mut r.d, a, &raw(b"hi"));
    assert_eq!(r.frame().as_deref(), Some(&b"hi"[..]));
    assert_eq!(got(&r.a).await, None);
    assert_eq!(r.d.mig.validations_started, 0);
}

/// An unknown CID: dropped and counted, nothing forwarded, nothing sent
/// back — from anywhere.
#[tokio::test]
async fn an_unknown_cid_is_dropped_and_counted() {
    let mut r = rig(4).await;
    let b = addr(&r.b);
    let stray = tag(CID ^ 1, &encode_raw(&FrameBody::new(1000, Bytes::new())));
    feed(&mut r.d, b, &stray);
    feed(&mut r.d, b, &encode_path_response(CID ^ 1, 9));
    assert_eq!(r.d.mig.cid_unknown, 2);
    assert_eq!(r.frame(), None);
    assert_eq!(got(&r.b).await, None);
    assert_eq!(r.d.sessions.key_at(&addr(&r.a)), Some(r.key), "untouched");
}

/// The whole migration (NAT rebinding seen from the demux): a tagged
/// datagram from B is delivered at once and B is challenged; everything
/// else — the ACK of a control frame that came FROM B included — still
/// goes to A (decision 10); A keeps working; B's matching response moves
/// the session and tells the writer, after which A is no one's.
#[tokio::test]
async fn a_validated_new_address_takes_the_session() {
    let mut r = rig(4).await;
    let (a, b) = (addr(&r.a), addr(&r.b));
    feed(&mut r.d, b, &raw(b"from b"));
    assert_eq!(r.frame().as_deref(), Some(&b"from b"[..]), "accepted");
    let nonce = nonce_of(&got(&r.b).await.expect("B is challenged"));
    assert_eq!(r.d.sessions.key_at(&a), Some(r.key), "still at A");

    let rel = tag(CID, &rel_frame(1, 7, b"hb"));
    feed(&mut r.d, b, &rel);
    assert_eq!(r.frame(), Some(b"hb".to_vec()), "control from B, delivered");
    assert_eq!(got(&r.a).await, Some(encode_ack(2)), "the ACK goes to A");
    assert_eq!(got(&r.b).await, None, "nothing but the challenge to B");
    feed(&mut r.d, a, &raw(b"from a"));
    assert_eq!(r.frame(), Some(b"from a".to_vec()), "A keeps working");

    feed(&mut r.d, b, &encode_path_response(CID, nonce));
    let notice = r.outbox.try_recv().expect("the writer is told");
    assert_eq!(notice[0].op, gsb_protocol::op::base::UDP_PATH);
    assert_eq!(decode_addr(&notice[0].payload), Some(b));
    assert_eq!(r.d.sessions.key_at(&b), Some(r.key), "moved");
    assert_eq!(r.d.sessions.key_at(&a), None, "A is no one's");
    assert_eq!(r.d.sessions.get(r.key).unwrap().path, None);
    let m = r.d.mig;
    assert_eq!((m.validations_started, m.migrations), (1, 1));
    assert_eq!(m.migrations_port_only, 1, "same IP, another port");
    assert_eq!(m.challenges_sent, 1, "one per resend interval");

    feed(&mut r.d, a, &raw(b"late a"));
    assert_eq!(
        r.frame(),
        Some(b"late a".to_vec()),
        "a tagged straggler: by CID"
    );
    assert_eq!(r.d.mig.validations_started, 2, "A is now a candidate");
}

/// A spoofed source with a valid CID that never answers: the session
/// stays on A, the validation times out (counted), and A's traffic was
/// never interrupted. A response for the wrong nonce, or from the wrong
/// address, is unmatched.
#[tokio::test]
async fn an_unanswered_challenge_times_out_and_the_old_path_stays() {
    let mut r = rig(4).await;
    let (a, b, c) = (addr(&r.a), addr(&r.b), addr(&r.c));
    feed(&mut r.d, b, &raw(b"spoof"));
    let nonce = nonce_of(&got(&r.b).await.unwrap());
    feed(&mut r.d, b, &encode_path_response(CID, nonce ^ 1));
    feed(&mut r.d, c, &encode_path_response(CID, nonce));
    assert_eq!(r.d.mig.responses_unmatched, 2);
    feed(&mut r.d, a, &raw(b"a"));
    r.d.path_expiry(r.key, Instant::now() + VALIDATION_TIMEOUT);
    assert_eq!(r.d.mig.validations_timed_out, 1);
    feed(&mut r.d, b, &encode_path_response(CID, nonce));
    assert_eq!(r.d.mig.responses_unmatched, 3, "too late");
    assert_eq!(r.d.sessions.key_at(&a), Some(r.key));
    assert_eq!(r.d.mig.migrations, 0);
    assert!(r.outbox.try_recv().is_err(), "the writer heard nothing");
}

/// A third address supersedes a pending validation (the newest wins);
/// an address that is another session's is never a candidate.
#[tokio::test]
async fn the_newest_candidate_wins_and_a_taken_address_is_refused() {
    let mut r = rig(4).await;
    let (b, c) = (addr(&r.b), addr(&r.c));
    feed(&mut r.d, b, &raw(b"1"));
    let old = nonce_of(&got(&r.b).await.unwrap());
    feed(&mut r.d, c, &raw(b"2"));
    let new = nonce_of(&got(&r.c).await.unwrap());
    assert_eq!(r.d.mig.validations_superseded, 1);
    feed(&mut r.d, b, &encode_path_response(CID, old));
    assert_eq!(r.d.mig.responses_unmatched, 1, "B's is over");
    let (in_tx, _i) = gsb_core::channel::channel(1);
    let (out_tx, _o) = gsb_core::channel::channel(1);
    r.d.sessions
        .insert(UdpSession::new(b, None, in_tx, out_tx, Instant::now()));
    feed(&mut r.d, b, &raw(b"3"));
    assert_eq!(r.d.mig.address_in_use, 1);
    assert_eq!(got(&r.b).await, None, "no challenge to another's address");
    feed(&mut r.d, c, &encode_path_response(CID, new));
    assert_eq!(r.d.sessions.key_at(&c), Some(r.key), "C still validates");
}

mod limits;
