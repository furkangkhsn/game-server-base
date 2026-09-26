//! The TLS handshake runs off the accept loop (BACKLOG B31): a peer
//! that never sends its hello holds one handshake slot, never the door,
//! and a failed handshake is not an accept error.

use super::*;

/// Far below the 10 s handshake deadline, far above a local handshake.
const PROMPT: Duration = Duration::from_secs(2);

async fn bind(pki: &TestPki) -> Arc<dyn Listener> {
    Arc::new(transport_for(pki))
        .bind("127.0.0.1:0".parse().unwrap())
        .await
        .expect("bind")
}

/// One accept in the background (the server's accept loop).
fn accept_one(listener: &Arc<dyn Listener>) -> tokio::task::JoinHandle<io::Result<SocketAddr>> {
    let l = listener.clone();
    tokio::spawn(async move { l.accept().await.map(|e| e.peer().expect("a peer")) })
}

/// A real TLS client, connected and verified against the minted CA.
async fn tls_client(pki: &TestPki, addr: SocketAddr) -> io::Result<SocketAddr> {
    let name: rustls::pki_types::ServerName<'static> = "localhost".try_into().expect("dns name");
    let tcp = tokio::net::TcpStream::connect(addr).await?;
    let local = tcp.local_addr()?;
    let tls = client_connector(pki).connect(name, tcp).await?;
    drop(tls);
    Ok(local)
}

/// A socket that never sends its ClientHello is accepted first; a real
/// TLS client behind it still completes its handshake — and reaches the
/// accept loop — at once.
#[tokio::test]
async fn an_idle_peer_does_not_hold_the_door() {
    let pki = mint_pki("idle");
    let listener = bind(&pki).await;
    let addr = listener.local_addr().unwrap();
    let _idle = tokio::net::TcpStream::connect(addr).await.expect("connect");
    tokio::time::sleep(Duration::from_millis(50)).await;
    let accepted = accept_one(&listener);
    let local = tokio::time::timeout(PROMPT, tls_client(&pki, addr))
        .await
        .expect("the handshake completes while the idle peer holds its own")
        .expect("a verified TLS session");
    let peer = tokio::time::timeout(PROMPT, accepted)
        .await
        .expect("the accept loop gets the TLS peer")
        .expect("no panic")
        .expect("an endpoint, not an error");
    assert_eq!(peer, local);
}

/// A failed handshake (a plaintext probe) is the client's: the accept
/// loop never sees it and gets the next good peer.
#[tokio::test]
async fn a_failed_handshake_is_not_an_accept_error() {
    let pki = mint_pki("failed");
    let listener = bind(&pki).await;
    let addr = listener.local_addr().unwrap();
    let accepted = accept_one(&listener);
    let mut probe = tokio::net::TcpStream::connect(addr).await.expect("connect");
    tokio::io::AsyncWriteExt::write_all(&mut probe, b"GET / HTTP/1.1\r\n\r\n")
        .await
        .expect("write");
    let mut sink = Vec::new();
    let _ = tokio::time::timeout(
        PROMPT,
        tokio::io::AsyncReadExt::read_to_end(&mut probe, &mut sink),
    )
    .await
    .expect("the door hangs up on the probe");
    let local = tokio::time::timeout(PROMPT, tls_client(&pki, addr))
        .await
        .expect("the good peer's handshake completes")
        .expect("a verified TLS session");
    let peer = tokio::time::timeout(PROMPT, accepted)
        .await
        .expect("the accept returns")
        .expect("no panic")
        .expect("the good peer, not the probe's error");
    assert_eq!(peer, local);
}

/// `close` cuts every handshake in flight: the silent peer's socket is
/// closed at once, with no accept pending at all.
#[tokio::test]
async fn close_cuts_the_handshakes_in_flight() {
    let pki = mint_pki("close-cuts");
    let listener = bind(&pki).await;
    let addr = listener.local_addr().unwrap();
    let mut idle = tokio::net::TcpStream::connect(addr).await.expect("connect");
    tokio::time::sleep(Duration::from_millis(50)).await;
    listener.close();
    let mut byte = [0u8; 1];
    let read = tokio::time::timeout(PROMPT, tokio::io::AsyncReadExt::read(&mut idle, &mut byte))
        .await
        .expect("the close ended the handshake in flight");
    assert!(
        matches!(read, Ok(0)) || read.is_err(),
        "EOF or reset, got {read:?}"
    );
}
