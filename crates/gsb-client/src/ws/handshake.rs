//! The client side of the RFC 6455 opening handshake: a random
//! `Sec-WebSocket-Key`, the upgrade request, and the checks on the 101
//! answer (status, `Upgrade`, `Connection`, the accept key, nothing
//! negotiated that was not asked for).

use std::io;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use bytes::BytesMut;
use sha1::{Digest, Sha1};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// RFC 6455 §1.3: appended to the key before hashing.
const MAGIC_GUID: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";

/// The most response head this client buffers (the door answers in a
/// few dozen bytes; the door's own request cap is the same 8 KiB).
const MAX_RESPONSE_HEAD: usize = 8 * 1024;

/// `Sec-WebSocket-Accept` for a `Sec-WebSocket-Key`:
/// base64(SHA-1(key + GUID)) (RFC 6455 §4.2.2).
pub fn accept_key(key: &str) -> String {
    let mut sha = Sha1::new();
    sha.update(key.as_bytes());
    sha.update(MAGIC_GUID.as_bytes());
    STANDARD.encode(sha.finalize())
}

/// A fresh key: 16 OS-random bytes, base64 (RFC 6455 §4.1).
fn new_key() -> io::Result<String> {
    let mut nonce = [0u8; 16];
    getrandom::fill(&mut nonce).map_err(|e| io::Error::other(format!("OS entropy: {e}")))?;
    Ok(STANDARD.encode(nonce))
}

fn refused(why: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, why.into())
}

/// Send the upgrade request and check the answer. Returns what the
/// server sent after the response head (the first frames, possibly).
pub(crate) async fn upgrade<S>(io: &mut S, host: &str, path: &str) -> io::Result<BytesMut>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let key = new_key()?;
    let request = format!(
        "GET {path} HTTP/1.1\r\n\
         Host: {host}\r\n\
         Upgrade: websocket\r\n\
         Connection: Upgrade\r\n\
         Sec-WebSocket-Key: {key}\r\n\
         Sec-WebSocket-Version: 13\r\n\
         \r\n"
    );
    io.write_all(request.as_bytes()).await?;
    io.flush().await?;

    let mut buf = BytesMut::with_capacity(1024);
    let end = loop {
        if let Some(at) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break at;
        }
        if buf.len() > MAX_RESPONSE_HEAD {
            return Err(refused("the upgrade response head exceeds 8 KiB"));
        }
        if io.read_buf(&mut buf).await? == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "the server closed before answering the upgrade",
            ));
        }
    };
    let head = buf.split_to(end + 4);
    check_response(&String::from_utf8_lossy(&head), &key)?;
    Ok(buf)
}

/// The 101 answer, checked against the key this client sent.
pub(super) fn check_response(head: &str, key: &str) -> io::Result<()> {
    let mut lines = head.split("\r\n");
    let status = lines.next().unwrap_or("");
    let mut parts = status.split_whitespace();
    if parts.next() != Some("HTTP/1.1") || parts.next() != Some("101") {
        return Err(refused(format!(
            "the server refused the upgrade: `{status}`"
        )));
    }
    let (mut upgrade, mut connection, mut accept) = (None, None, None);
    for line in lines.filter(|l| !l.is_empty()) {
        let Some((name, value)) = line.split_once(':') else {
            return Err(refused(format!("malformed header line `{line}`")));
        };
        let value = value.trim();
        match name.trim().to_ascii_lowercase().as_str() {
            "upgrade" => upgrade = Some(value),
            "connection" => connection = Some(value),
            "sec-websocket-accept" => accept = Some(value),
            // §4.1: a subprotocol or an extension this client did not
            // offer fails the connection.
            "sec-websocket-protocol" | "sec-websocket-extensions" => {
                return Err(refused(format!("unrequested `{line}`")));
            }
            _ => {}
        }
    }
    if !upgrade.is_some_and(|u| u.eq_ignore_ascii_case("websocket")) {
        return Err(refused("the 101 lacks `Upgrade: websocket`"));
    }
    let upgrades = connection.is_some_and(|c| {
        c.split(',')
            .any(|t| t.trim().eq_ignore_ascii_case("upgrade"))
    });
    if !upgrades {
        return Err(refused("the 101 lacks `Connection: upgrade`"));
    }
    if accept != Some(accept_key(key).as_str()) {
        return Err(refused(format!(
            "Sec-WebSocket-Accept {accept:?} does not answer this key"
        )));
    }
    Ok(())
}
