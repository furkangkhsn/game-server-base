//! The client's record layer (B5a): a sealed session opens only records,
//! every refusal counted under its name; its own datagrams are records
//! the server opens; and during the handshake a forged message 2 or a
//! plaintext accept neither ends the handshake nor establishes it.

use super::*;
use crate::seal::{Accept, Initiator, Msg1, Opener, ResetToken, Sealer, StaticKey};
use crate::udp::sealed::{context, encode_sealed_accept};

/// A finished NK handshake: (server sealer, server opener, client
/// sealer, client opener).
pub(super) fn session_pair() -> (Sealer, Opener, Sealer, Opener) {
    let server = StaticKey::generate().unwrap();
    let mut ini = Initiator::new(&server.public(), b"ctx", &[]).unwrap();
    let accept = Accept {
        cid: 11,
        reset_token: ResetToken::from_bytes([0; 16]),
    };
    let r = Msg1::parse(ini.msg1())
        .unwrap()
        .cookie_verified(&server, b"ctx", &accept)
        .unwrap();
    let (_, client) = ini.finish(&r.msg2).unwrap();
    let (ss, so) = r.session.into_halves();
    let (cs, co) = client.into_halves();
    (ss, so, cs, co)
}

pub(super) fn seal(s: &mut Sealer, inner: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    s.seal(inner, &mut out).unwrap();
    out
}

/// What the client reads: a genuine record's frame; a replayed, a
/// tampered and an unsealed datagram are dropped, each counted under its
/// name. The client's ACK for a REL goes out as a record the server
/// opens.
#[tokio::test]
async fn a_sealed_client_reads_only_records_and_counts_every_refusal() {
    let (mut ss, mut so, cs, co) = session_pair();
    let config = UdpClientConfig {
        server_key: Some([1; 32]),
        ..UdpClientConfig::default()
    };
    let (mut c, sink) = detached_with(config).await;
    c.seal.install(cs, co, ResetToken::from_bytes([0; 16]));
    let raw = encode_raw(&FrameBody::new(1000, Bytes::from_static(b"snap")));
    let rec = seal(&mut ss, &raw);
    assert!(c.process_datagram(&rec), "a genuine record is read");
    assert_eq!(c.raw.take().unwrap().payload.as_ref(), b"snap");
    assert!(!c.process_datagram(&rec), "its replay is not");
    let mut bad = seal(&mut ss, &raw);
    bad[12] ^= 1;
    assert!(!c.process_datagram(&bad));
    assert!(!c.process_datagram(&raw), "plaintext on a sealed session");
    let s = &c.stats;
    assert_eq!(
        (s.seal_replayed, s.seal_forged, s.unsealed_dropped),
        (1, 1, 1)
    );

    let rel = encode_rel(1, &FrameBody::new(8, Bytes::from_static(b"ok")));
    c.process_datagram(&seal(&mut ss, &rel));
    let mut buf = [0u8; 256];
    let (n, _) = tokio::time::timeout(Duration::from_secs(3), sink.recv_from(&mut buf))
        .await
        .expect("the client's ACK")
        .expect("recv");
    let ack = so.open(&buf[..n]).expect("a record the server opens");
    assert_eq!(ack.plaintext, encode_ack(2));
    assert_eq!(buf[1..9], 11u64.to_le_bytes(), "its CID in the header");
}

/// The handshake against a hand-driven server: a forged message 2 and a
/// plaintext accept are refused and counted, the handshake goes on, and
/// the genuine accept establishes it — with message 2's CID.
#[tokio::test]
async fn a_forged_or_plaintext_accept_does_not_end_the_sealed_handshake() {
    let server = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let addr = server.local_addr().unwrap();
    let key = StaticKey::generate().unwrap();
    let config = UdpClientConfig {
        server_key: Some(key.public()),
        ..UdpClientConfig::default()
    };
    let client = tokio::spawn(UdpClient::connect_within_with(
        addr,
        Duration::from_secs(3),
        config,
    ));
    let mut buf = [0u8; 256];
    let (n, from) = server.recv_from(&mut buf).await.unwrap();
    assert_eq!(n, 18);
    let nonce = u64_at(&buf, 1).unwrap();
    server
        .send_to(&encode_hello(nonce, 77), from)
        .await
        .unwrap();
    let (n, _) = server.recv_from(&mut buf).await.unwrap();
    assert_eq!(n, 67, "the sealed proof");
    let proof = buf[..n].to_vec();
    server
        .send_to(&encode_sealed_accept(&[0x11; crate::seal::MSG2_LEN]), from)
        .await
        .unwrap();
    server.send_to(&encode_ack(1), from).await.unwrap();
    let accept = Accept {
        cid: 5,
        reset_token: ResetToken::from_bytes([2; 16]),
    };
    let r = Msg1::parse(&proof[19..])
        .unwrap()
        .cookie_verified(&key, &context(nonce, 77), &accept)
        .unwrap();
    server
        .send_to(&encode_sealed_accept(&r.msg2), from)
        .await
        .unwrap();
    let c = client
        .await
        .unwrap()
        .expect("established by the genuine one");
    assert!(c.is_established() && c.sealed());
    assert_eq!(c.path.cid, Some(5));
    assert_eq!((c.stats.accepts_forged, c.stats.accepts_unsealed), (1, 1));
}

/// Key phases, client side (B5b): the client → server key moves to its
/// next generation only once the server's ACK covered a control frame
/// first sent in the current phase; the server's opener follows.
#[tokio::test]
async fn a_sealed_client_rekeys_once_the_server_acks_the_phase() {
    let (mut ss, mut so, cs, co) = session_pair();
    let config = UdpClientConfig {
        server_key: Some([1; 32]),
        rekey: RekeyPolicy {
            after: Duration::ZERO,
            after_records: u64::MAX,
        },
        ..UdpClientConfig::default()
    };
    let (mut c, sink) = detached_with(config).await;
    c.seal.install(cs, co, ResetToken::from_bytes([0; 16]));
    let mut buf = [0u8; 256];
    let mut next = async |so: &mut Opener| {
        let (n, _) = tokio::time::timeout(Duration::from_secs(3), sink.recv_from(&mut buf))
            .await
            .expect("a record")
            .expect("recv");
        so.open(&buf[..n]).expect("the server opens it")
    };
    c.send_frame(8, Bytes::from_static(b"hb")).await.unwrap();
    assert_eq!(next(&mut so).await.counter, 0, "REL seq 1");
    let half = c.seal.send_half().unwrap();
    half.sealer_mut()
        .set_next_counter_for_test(crate::seal::REKEY_MIN_DISTANCE);
    c.send_frame(1000, Bytes::from_static(b"g")).await.unwrap();
    next(&mut so).await;
    assert_eq!((c.stats.rekeys, c.stats.rekeys_unconfirmed), (0, 1));
    // The server's ACK: seq 1 arrived.
    c.process_datagram(&seal(&mut ss, &encode_ack(2)));
    c.send_frame(1000, Bytes::from_static(b"g")).await.unwrap();
    next(&mut so).await;
    assert_eq!((c.stats.rekeys, c.stats.rekeys_unconfirmed), (1, 1));
    assert_eq!(so.generation(), 1, "the server's opener followed");
}
