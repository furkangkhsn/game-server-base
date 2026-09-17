//! Demux unit tests: a Demux driven directly, no socket. A child of
//! `demux`, so the actor's private session state stays private to
//! its own module tree.

use super::*;
use crate::udp::wire::*;
use bytes::Bytes;
use gsb_protocol::FrameBody;

/// Direct-demux harness: a demux on a bound socket with one session
/// pre-installed (handshake skipped), its inbound mailbox exposed.
fn demux_with_session(
    sock: Arc<UdpSocket>,
    peer: SocketAddr,
) -> (Demux, gsb_core::channel::Inbox<gsb_core::conn::ConnIn>) {
    let (end_tx, _end_rx) = crossbeam_channel::bounded(4);
    let (in_tx, in_rx) = gsb_core::channel::channel(16);
    let (out_tx, _out_rx) = gsb_core::channel::channel(16);
    let mut d = Demux {
        sock,
        end_tx,
        cookie: CookieKey::generate().expect("OS entropy in test"),
        inbox_cap: 16,
        outbox_cap: 16,
        max_datagram: DEFAULT_MAX_DATAGRAM_BYTES,
        idle: None,
        sessions: HashMap::new(),
        deadlines: BTreeSet::new(),
        buf: vec![0u8; 65536],
        established: 1,
        challenges: 0,
        bad_cookie: 0,
        endpoints_dropped: 0,
        swept_idle: 0,
        removed_actor_gone: 0,
        acks_piggybacked: 0,
        ack_piggyback_failed: 0,
        oversized_in: 0,
        bad_datagrams: 0,
    };
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
