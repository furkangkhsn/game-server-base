//! Certificate handling: the self-signed path, a wrong CA that must
//! fail the handshake, and a missing cert file that must fail the
//! bind rather than fall back to anything.

use super::*;

/// Self-signed certs minted at runtime (rcgen) bind cleanly, and a
/// client trusting the minted CA connects and sees the peer address.
#[tokio::test]
async fn self_signed_certs_connect_successfully() {
    let pki = mint_pki("self-signed");
    let listener = Arc::new(transport_for(&pki))
        .bind("127.0.0.1:0".parse().unwrap())
        .await
        .expect("bind");
    let bound = listener.local_addr().unwrap();

    let server = tokio::spawn(async move {
        let endpoint = listener.accept().await.expect("accept");
        endpoint.peer()
    });
    let (reader, mut writer) = connect(bound, "localhost", pki.ca_pem.as_bytes())
        .await
        .expect("connect");
    // A bare `open_bi` puts nothing on the wire: QUIC is lazy, the
    // stream-open frame rides the next outgoing packet, so a client
    // that opens the stream and then stays SILENT would — by design —
    // hold the server inside [`HANDSHAKE_TIMEOUT`] (see the module
    // docs). Close the writer cleanly: the FIN carries the stream-open
    // across, the server's accept completes, and the rcgen-minted
    // cert path is proven end to end.
    writer.close().await.expect("clean client close");
    let peer = server.await.expect("server task").expect("peer recorded");
    assert_eq!(peer.ip().to_string(), "127.0.0.1");
    drop(reader);
}

/// A client trusting a DIFFERENT CA fails the handshake cleanly (the
/// server reports a failed handshake; the client gets a connect
/// error) — QUIC's TLS layer rejects before any stream can open.
#[tokio::test]
async fn wrong_ca_fails_the_handshake() {
    let pki = mint_pki("wrong-ca");
    let other = mint_pki("other-ca");
    let listener = Arc::new(transport_for(&pki))
        .bind("127.0.0.1:0".parse().unwrap())
        .await
        .expect("bind");

    let bound = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        // The server observes the failed handshake as an accept error.
        assert!(
            listener.accept().await.is_err(),
            "handshake must fail server-side"
        );
    });

    let result = connect(bound, "localhost", other.ca_pem.as_bytes()).await;
    assert!(result.is_err(), "client must reject the unknown CA");
    server.await.expect("server sees the failure");
}

/// A bind with a nonexistent cert file fails with the path named.
#[tokio::test]
async fn missing_cert_file_fails_the_bind() {
    let transport = QuicTransport {
        config: QuicTransportConfig {
            cert_chain_pem: "/nonexistent/gsb-quic/cert.pem".into(),
            key_pem: "/nonexistent/gsb-quic/key.pem".into(),
            max_frame_bytes: crate::tcp::DEFAULT_MAX_FRAME_BYTES,
        },
    };
    let result = Arc::new(transport)
        .bind("127.0.0.1:0".parse().unwrap())
        .await;
    let Err(err) = result else {
        panic!("bind with a missing cert file must fail");
    };
    let msg = err.to_string();
    assert!(
        msg.contains("/nonexistent/gsb-quic/cert.pem"),
        "the error must name the file: {msg}"
    );
}
