//! The write-stall clock over the QUIC door: bytes, not frames.
//!
//! Same property as `tcp::tests::slow_reader`, forced the way the
//! server-level write-stall suite forces its deaf peer: QUIC is the door
//! whose receive window the CLIENT sets, so "reads slowly" is exact —
//! the server can never be more than the advertised window ahead of the
//! client's reads, with no kernel buffer to hide in. One frame takes
//! several windows to drain; a peer reading all along must keep its
//! session.

use super::*;

use std::time::{Duration, Instant};

use crate::pump::PumpTimeouts;

/// A whole second: the client's pace (below) must stay far from the
/// window even when the machine stalls the test process (BACKLOG F25).
const WINDOW: Duration = Duration::from_secs(1);
/// One frame: at least 384 reads of at most [`READ_CHUNK`], each after a
/// [`READ_EVERY`] sleep — ≥ 3.8 s, i.e. several windows, on any machine.
const FRAME: usize = 384 * 1024;
/// The client's per-stream flow-control window: the most the server can
/// be ahead of what the client has read.
const CLIENT_WINDOW: u32 = 2048;
const READ_CHUNK: usize = 1024;
const READ_EVERY: Duration = Duration::from_millis(10);

/// A QUIC client advertising a tiny receive window; sends one frame so
/// the server's `accept_bi` completes (QUIC opens streams lazily).
async fn slow_client(addr: SocketAddr, ca_pem: &str) -> (quinn::SendStream, quinn::RecvStream) {
    let mut roots = rustls::RootCertStore::empty();
    for der in rustls_pemfile::certs(&mut ca_pem.as_bytes()) {
        roots.add(der.expect("CA PEM")).expect("CA parses");
    }
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut tls = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .expect("protocol versions")
        .with_root_certificates(roots)
        .with_no_client_auth();
    tls.alpn_protocols = vec![ALPN_PROTOCOL.to_vec()];
    let crypto = quinn::crypto::rustls::QuicClientConfig::try_from(tls).expect("QUIC TLS");
    let mut client_config = quinn::ClientConfig::new(Arc::new(crypto));
    let mut transport = quinn::TransportConfig::default();
    transport.stream_receive_window(CLIENT_WINDOW.into());
    transport.receive_window((CLIENT_WINDOW * 4).into());
    client_config.transport_config(Arc::new(transport));
    let mut endpoint = quinn::Endpoint::client("127.0.0.1:0".parse().unwrap()).expect("endpoint");
    endpoint.set_default_client_config(client_config);
    let conn = endpoint
        .connect(addr, "localhost")
        .expect("connect setup")
        .await
        .expect("QUIC handshake");
    let (mut send, recv) = conn.open_bi().await.expect("bi-stream");
    let mut writer = FrameWriter::new(&mut send);
    writer
        .send(FrameBody::new(7, b"hi".as_slice()))
        .await
        .expect("first frame");
    (send, recv)
}

/// THE REGRESSION LOCK on the QUIC door: the client reads 1 KiB every
/// 10 ms — never stopping — and the server must not end the session
/// while one 384 KiB frame takes several windows to drain.
#[tokio::test]
async fn a_slow_but_steady_quic_reader_survives_a_frame_longer_than_the_window() {
    let pki = mint_pki("slow-reader");
    let listener = Arc::new(transport_for(&pki))
        .bind("127.0.0.1:0".parse().unwrap())
        .await
        .expect("bind");
    let bound = listener.local_addr().unwrap();
    let ca = pki.ca_pem.clone();
    let client = tokio::spawn(async move { slow_client(bound, &ca).await });
    let endpoint = listener.accept().await.expect("accept");
    let (in_tx, mut in_rx) = channel::<ConnIn>(8);
    let (out_tx, out_rx) = channel::<FrameBatch>(4);
    let (read, write) = endpoint.start_pump(
        ConnectionId(41),
        in_tx,
        out_rx,
        PumpTimeouts {
            idle: None,
            write_stall: Some(WINDOW),
        },
    );
    let (_send, mut recv) = client.await.expect("client task");
    out_tx
        .send(vec![FrameBody::new(7, vec![0u8; FRAME])])
        .await
        .expect("the writer pump takes the frame");

    let want = 4 + 2 + FRAME;
    let started = Instant::now();
    let mut got = 0usize;
    let mut buf = vec![0u8; READ_CHUNK];
    while got < want {
        tokio::time::sleep(READ_EVERY).await;
        match tokio::time::timeout(Duration::from_secs(5), recv.read(&mut buf)).await {
            Ok(Ok(Some(n))) => got += n,
            Ok(other) => panic!(
                "the server ended the stream after {got} of {want} bytes \
                 ({:?} in), while this client was reading: {other:?}",
                started.elapsed()
            ),
            Err(_) => panic!("the writer stopped writing after {got} of {want} bytes"),
        }
    }
    let took = started.elapsed();
    assert!(
        took >= WINDOW * 3,
        "the frame drained in {took:?}; it must take several {WINDOW:?} windows"
    );
    // The client's own first frame is in the inbox; a close must not be.
    while let Ok(msg) = in_rx.try_recv() {
        assert!(
            matches!(msg, ConnIn::Frame(_)),
            "no close may be reported for a client that kept reading: {msg:?}"
        );
    }

    drop(out_tx);
    let _ = tokio::time::timeout(Duration::from_secs(5), write).await;
    if let Some(read) = read {
        read.abort();
    }
}
