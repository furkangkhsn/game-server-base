//! Demux unit tests: a Demux driven directly, no socket. A child of
//! `demux`, so the actor's private session state stays private to
//! its own module tree.

use super::*;
use crate::udp::wire::*;
use bytes::Bytes;
use gsb_protocol::FrameBody;

/// Direct-demux harness: a demux on a bound socket, no sessions, with
/// its endpoint receiver handed back (a DROPPED receiver would make every
/// handshake tear its own session down again — the "accept loop is gone"
/// arm).
fn demux_bare(sock: Arc<UdpSocket>) -> (Demux, crossbeam_channel::Receiver<Queued>) {
    let (end_tx, end_rx) = crossbeam_channel::bounded(4);
    let mut d = Demux::new(
        sock,
        end_tx,
        CookieKey::generate().expect("OS entropy in test"),
        16,
        16,
        DEFAULT_MAX_DATAGRAM_BYTES,
        None,
    );
    d.buf = vec![0u8; 65536];
    (d, end_rx)
}

/// Direct-demux harness: a demux on a bound socket with one session
/// pre-installed (handshake skipped), its inbound mailbox exposed.
fn demux_with_session(
    sock: Arc<UdpSocket>,
    peer: SocketAddr,
) -> (Demux, gsb_core::channel::Inbox<gsb_core::conn::ConnIn>) {
    let (in_tx, in_rx) = gsb_core::channel::channel(16);
    let (out_tx, _out_rx) = gsb_core::channel::channel(16);
    let (mut d, _end_rx) = demux_bare(sock);
    d.established = 1;
    d.sessions.insert(
        peer,
        UdpSession {
            in_tx,
            out_tx,
            last_seen: Instant::now(),
            in_expected: 1,
            in_oob: HashMap::new(),
            oob_dropped: 0,
            dup_in: 0,
            inbox_full: 0,
            inbox_full_warned: false,
        },
    );
    (d, in_rx)
}

/// Feed one crafted datagram to the demux (bypassing the socket).
fn feed(d: &mut Demux, peer: SocketAddr, datagram: &[u8]) {
    let n = datagram.len().min(d.buf.len());
    d.buf[..n].copy_from_slice(&datagram[..n]);
    d.handle(n, peer);
}

fn rel_frame(seq: u32, op: u16, payload: &[u8]) -> Vec<u8> {
    encode_rel(seq, &FrameBody::new(op, Bytes::copy_from_slice(payload)))
}

/// Inbound reliable band: an out-of-order datagram is buffered behind
/// the gap and delivered in ORDER once the gap fills; a duplicate is
/// re-ACKed but never re-forwarded.
#[tokio::test]
async fn inbound_rel_reorders_and_dedupes() {
    let sock = Arc::new(
        UdpSocket::bind("127.0.0.1:0".parse::<SocketAddr>().unwrap())
            .await
            .expect("bind"),
    );
    let peer = "127.0.0.1:9".parse().unwrap();
    let (mut d, mut in_rx) = demux_with_session(sock, peer);

    // seq 2 arrives FIRST (the gap): buffered, nothing forwarded, no
    // ACK advance.
    feed(&mut d, peer, &rel_frame(2, 1000, b"second"));
    assert!(in_rx.is_empty(), "a gapped frame must wait for order");

    // seq 1 fills the gap: BOTH frames flow, in order.
    feed(&mut d, peer, &rel_frame(1, 1000, b"first"));
    let f1 = in_rx.try_recv().expect("first frame");
    let f2 = in_rx.try_recv().expect("second frame");
    assert!(in_rx.is_empty());
    match (&f1, &f2) {
        (gsb_core::conn::ConnIn::Frame(a), gsb_core::conn::ConnIn::Frame(b)) => {
            assert_eq!(a.payload.as_ref(), b"first");
            assert_eq!(b.payload.as_ref(), b"second");
        }
        _ => panic!("expected two frames"),
    }

    // Duplicate seq 1: re-ACKed (the demux would have sent an ACK
    // datagram on the socket — not observable here), but NOT
    // re-forwarded.
    feed(&mut d, peer, &rel_frame(1, 1000, b"first"));
    assert!(in_rx.is_empty(), "duplicates must never be re-forwarded");
}

/// Cookie rotation, at the demux: a proof minted two slots ago is
/// REJECTED (a captured proof expires), while one minted in the previous
/// slot — a handshake in flight across a rotation boundary — still
/// establishes the session. The wire is untouched: both are the same
/// 18-byte HELLO the client has always sent.
#[tokio::test]
async fn handshake_expires_a_captured_proof_but_crosses_one_rotation() {
    let sock = Arc::new(
        UdpSocket::bind("127.0.0.1:0".parse::<SocketAddr>().unwrap())
            .await
            .expect("bind"),
    );
    let (mut d, end_rx) = demux_bare(sock);
    // A server that has been up for five rotations.
    d.clock = CookieClock::started_at(Instant::now() - COOKIE_SLOT * 5);
    assert_eq!(d.clock.slot(), 5, "the harness must sit in slot 5");

    let peer: SocketAddr = "127.0.0.1:41000".parse().unwrap();
    let nonce = 0xFEED_FACE_u64;

    // Slot 3: two rotations old. Dropped, counted, answered with nothing.
    let stale = d.cookie.compute(nonce, peer, 3);
    feed(&mut d, peer, &encode_hello(nonce, stale));
    assert!(
        !d.sessions.contains_key(&peer),
        "an expired proof must not establish a session"
    );
    assert!(
        end_rx.is_empty(),
        "an expired proof must not yield an endpoint"
    );
    assert_eq!(d.bad_cookie, 1, "the expired proof must be counted");

    // Slot 4: the previous slot — the in-flight grace. Accepted.
    let in_flight = d.cookie.compute(nonce, peer, 4);
    feed(&mut d, peer, &encode_hello(nonce, in_flight));
    assert!(
        d.sessions.contains_key(&peer),
        "a proof one rotation old must still establish (a handshake can \
         cross a boundary)"
    );
    assert_eq!(d.established, 1);
    assert_eq!(
        d.bad_cookie, 1,
        "the accepted proof must not be counted bad"
    );
    assert!(
        end_rx.try_recv().is_ok(),
        "the session must reach the accept loop"
    );
}

/// Inbound fragments are REFUSED: the server never reassembles, so a
/// FRAG datagram from an established session forwards nothing and holds
/// nothing — it is counted and forgotten — and the session's next RAW
/// frame is delivered as usual.
#[tokio::test]
async fn inbound_fragments_are_refused() {
    let sock = Arc::new(
        UdpSocket::bind("127.0.0.1:0".parse::<SocketAddr>().unwrap())
            .await
            .expect("bind"),
    );
    let peer = "127.0.0.1:9".parse().unwrap();
    let (mut d, mut in_rx) = demux_with_session(sock, peer);

    // A well-formed two-fragment message, both halves.
    feed(&mut d, peer, &[KIND_FRAG, 0, 0, 0, 2, 0xE8, 0x03]);
    feed(&mut d, peer, &[KIND_FRAG, 0, 0, 1, 2, 7, 7]);
    assert!(
        in_rx.is_empty(),
        "no fragment and no reassembly reaches the actor"
    );
    assert_eq!(d.frag_refused, 2);
    assert_eq!(d.bad_datagrams, 0, "refused by rule, not malformed");

    let raw = encode_raw(&FrameBody::new(1000, Bytes::from_static(b"move")));
    feed(&mut d, peer, &raw);
    match in_rx.try_recv().expect("the RAW frame") {
        gsb_core::conn::ConnIn::Frame(f) => assert_eq!(f.payload.as_ref(), b"move"),
        other => panic!("expected a frame, got {other:?}"),
    }
}

mod closed;
mod full;
mod gone;
mod handshake;
mod reap;
mod report;
