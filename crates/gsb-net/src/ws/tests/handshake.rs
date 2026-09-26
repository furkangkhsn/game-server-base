//! The HTTP upgrade's rejection paths: every malformed or
//! non-WebSocket request must get a clean 400, never a half-open
//! socket.

use super::*;

/// Malformed request heads get an HTTP 400 and a closed socket (the
/// door counts a failed handshake, like the TLS listener's).
#[tokio::test]
async fn malformed_handshake_gets_http_400() {
    let addr = serve_echo(None).await;
    let mut client = FakeWsClient::raw(addr).await;
    client.write_all(b"NONSENSE\r\n\r\n").await.unwrap();

    let head = read_http_head(&mut client).await;
    assert!(head.starts_with("HTTP/1.1 400 Bad Request"), "got: {head}");
    // Drain the short plain-text body (Content-Length delimited), then
    // the server must hang up (EOF, or a reset if it tore down first).
    let len: usize = header_value(&head, "content-length")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let mut body = vec![0u8; len];
    if len > 0 {
        client.read_exact(&mut body).await.expect("400 body");
    }
    let mut eof = [0u8; 1];
    match client.read(&mut eof).await {
        Ok(0) => {}
        Ok(n) => panic!("expected end of stream, got {n} byte(s)"),
        Err(e) if e.kind() == io::ErrorKind::ConnectionReset => {}
        Err(e) => panic!("unexpected read error at teardown: {e}"),
    }
}

/// A well-formed HTTP request that simply is not a websocket upgrade is
/// also a 400 (missing/mismatched headers).
#[tokio::test]
async fn non_websocket_get_request_gets_http_400() {
    let addr = serve_echo(None).await;
    let mut client = FakeWsClient::raw(addr).await;
    client
        .write_all(b"GET / HTTP/1.1\r\nHost: x\r\nAccept: */*\r\n\r\n")
        .await
        .unwrap();
    let head = read_http_head(&mut client).await;
    assert!(head.starts_with("HTTP/1.1 400 Bad Request"), "got: {head}");
}

/// Wrong Sec-WebSocket-Version → 400 (the header IS sent, just wrong).
#[tokio::test]
async fn wrong_version_gets_http_400() {
    let addr = serve_echo(None).await;
    let mut client = FakeWsClient::raw(addr).await;
    client
        .write_all(
            b"GET / HTTP/1.1\r\nHost: x\r\nUpgrade: websocket\r\n\
              Connection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\
              Sec-WebSocket-Version: 8\r\n\r\n",
        )
        .await
        .unwrap();
    let head = read_http_head(&mut client).await;
    assert!(head.starts_with("HTTP/1.1 400 Bad Request"), "got: {head}");
}

// ── data plane ──────────────────────────────────────────────────────
