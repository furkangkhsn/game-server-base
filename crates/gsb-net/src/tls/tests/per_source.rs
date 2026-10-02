//! The per-source handshake cap at a real door (BACKLOG D11): a source
//! at its cap has its next connection closed unhandshaken and counted;
//! another source is served; a slot its holder gives back is the
//! source's again; the count reaches the collector.

use gsb_core::metrics::MetricsEvent;

use super::*;
use crate::transport::intake::tests::until;

/// Far below the 10 s handshake deadline, far above a local handshake.
const PROMPT: Duration = Duration::from_secs(2);

/// A TCP connection from `source` (any 127/8 address is the loopback).
async fn connect_from(source: [u8; 4], to: SocketAddr) -> tokio::net::TcpStream {
    let socket = tokio::net::TcpSocket::new_v4().expect("socket");
    socket
        .bind(SocketAddr::from((source, 0)))
        .expect("bind the source");
    socket.connect(to).await.expect("connect")
}

/// A verified TLS session from `source`; its local address.
async fn tls_from(pki: &TestPki, source: [u8; 4], to: SocketAddr) -> io::Result<SocketAddr> {
    let name: rustls::pki_types::ServerName<'static> = "localhost".try_into().expect("dns name");
    let tcp = connect_from(source, to).await;
    let local = tcp.local_addr()?;
    let tls = client_connector(pki).connect(name, tcp).await?;
    drop(tls);
    Ok(local)
}

/// The door closes the connection without a byte: EOF or reset, at once.
async fn closed_at_once(stream: &mut tokio::net::TcpStream) {
    let mut byte = [0u8; 1];
    let read = tokio::time::timeout(PROMPT, tokio::io::AsyncReadExt::read(stream, &mut byte))
        .await
        .expect("closed at once, not held to the handshake deadline");
    assert!(matches!(read, Ok(0)) || read.is_err(), "got {read:?}");
}

#[tokio::test]
async fn a_source_at_its_cap_is_refused_and_another_is_served() {
    let pki = mint_pki("per-source");
    let (tx, mut rx) = tokio::sync::mpsc::channel(64);
    let mut transport = transport_for(&pki);
    transport.config.max_handshakes_per_source = Some(2);
    transport.config.metrics = Some(tx);
    let listener: Arc<dyn Listener> = Arc::new(transport)
        .bind("127.0.0.1:0".parse().unwrap())
        .await
        .expect("bind");
    let addr = listener.local_addr().unwrap();
    let stats = || listener.handshake_stats().expect("a handshaking door");

    let first = connect_from([127, 0, 0, 1], addr).await;
    let _second = connect_from([127, 0, 0, 1], addr).await;
    until("both in flight", || stats().in_flight == 2).await;
    let mut third = connect_from([127, 0, 0, 1], addr).await;
    closed_at_once(&mut third).await;
    until("the refusal counted", || stats().refused_per_source == 1).await;
    assert_eq!(
        (stats().refused, stats().in_flight),
        (0, 2),
        "{:?}",
        stats()
    );

    // Another source is served while the first holds its cap.
    let l = listener.clone();
    let accepted = tokio::spawn(async move { l.accept().await.map(|e| e.peer()) });
    let other = tokio::time::timeout(PROMPT, tls_from(&pki, [127, 0, 0, 2], addr))
        .await
        .expect("prompt")
        .expect("a verified session from another source");
    let peer = tokio::time::timeout(PROMPT, accepted)
        .await
        .expect("the accept returns")
        .expect("no panic")
        .expect("an endpoint");
    assert_eq!(peer, Some(other));

    // The first holder hangs up: its handshake fails, its slot is the
    // source's again.
    drop(first);
    until("the slot given back", || stats().in_flight == 1).await;
    tokio::time::timeout(PROMPT, tls_from(&pki, [127, 0, 0, 1], addr))
        .await
        .expect("prompt")
        .expect("the source is served again");
    assert_eq!(stats().refused_per_source, 1);

    // The door's last sample carries the refusal (B58).
    listener.close();
    let mut refused = 0;
    while refused < 1 {
        match tokio::time::timeout(PROMPT, rx.recv()).await {
            Ok(Some(MetricsEvent::Transport(t))) => refused += t.handshakes_refused_per_source,
            other => panic!("no refusal in the samples: {other:?}"),
        }
    }
    assert_eq!(refused, 1);
}
