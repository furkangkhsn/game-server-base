//! The HTTP/1.1 upgrade that turns a TCP stream into a WebSocket: the
//! `Sec-WebSocket-Accept` derivation (the one place sha1/base64 are
//! needed) and the request-head parse that guards it.

use std::io;

use bytes::BytesMut;
use sha1::Digest;
use sha1::Sha1;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;


use crate::ws::*;

/// Standard-alphabet base64 with padding, hand-rolled (~15 lines) so the
/// handshake needs no extra dependency. Only used for the 20-byte SHA-1
/// digest and unit-tested against the RFC 4648 vectors.
pub(super) fn base64_encode(data: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b1 = chunk[0] as u32;
        let b2 = *chunk.get(1).unwrap_or(&0) as u32;
        let b3 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (b1 << 16) | (b2 << 8) | b3;
        out.push(ALPHABET[(n >> 18 & 63) as usize] as char);
        out.push(ALPHABET[(n >> 12 & 63) as usize] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[(n >> 6 & 63) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[(n & 63) as usize] as char
        } else {
            '='
        });
    }
    out
}

/// `Sec-WebSocket-Accept` = base64(SHA-1(key + MAGIC_GUID)) (RFC 6455 §4.2.2).
pub(super) fn accept_key(client_key: &[u8]) -> String {
    let mut hasher = Sha1::new();
    hasher.update(client_key);
    hasher.update(MAGIC_GUID.as_bytes());
    base64_encode(&hasher.finalize())
}

/// Perform the server side of the RFC 6455 opening handshake on `stream`.
///
/// On success the stream is positioned right after the 101 response: only
/// WebSocket frames follow. On any failure a best-effort HTTP 400 has been
/// written and the error returned (the caller drops the socket, like the
/// TLS listener does for failed handshakes).
pub(super) async fn perform_upgrade(mut stream: TcpStream) -> io::Result<TcpStream> {
    let head = match read_request_head(&mut stream).await {
        Ok(head) => head,
        Err(e) => return reject_and_fail(stream, e).await,
    };
    let key = match parse_upgrade_request(&head) {
        Ok(key) => key,
        Err(why) => {
            return reject_and_fail(stream, io::Error::new(io::ErrorKind::InvalidData, why)).await;
        }
    };
    let response = format!(
        "HTTP/1.1 101 Switching Protocols\r\n\
         Upgrade: websocket\r\n\
         Connection: Upgrade\r\n\
         Sec-WebSocket-Accept: {}\r\n\
         \r\n",
        accept_key(key.as_bytes())
    );
    stream.write_all(response.as_bytes()).await?;
    stream.flush().await?;
    Ok(stream)
}

/// Write an HTTP 400 with a short plain-text reason, then surface `e`.
pub(super) async fn reject_and_fail(mut stream: TcpStream, e: io::Error) -> io::Result<TcpStream> {
    let body = format!("WebSocket handshake rejected: {}\n", e);
    let response = format!(
        "HTTP/1.1 400 Bad Request\r\n\
         Connection: close\r\n\
         Content-Type: text/plain; charset=utf-8\r\n\
         Content-Length: {}\r\n\
         \r\n{}",
        body.len(),
        body
    );
    // Best effort: the diagnostic matters more than write errors here.
    let _ = stream.write_all(response.as_bytes()).await;
    let _ = stream.flush().await;
    Err(e)
}

/// Read bytes until the `\r\n\r\n` end-of-head marker (or the cap).
pub(super) async fn read_request_head(stream: &mut TcpStream) -> io::Result<String> {
    let mut buf = BytesMut::with_capacity(1024);
    loop {
        if let Some(end) = find_head_end(&buf) {
            return Ok(String::from_utf8_lossy(&buf[..end]).into_owned());
        }
        if buf.len() >= MAX_REQUEST_HEAD_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("request head exceeds {} bytes", MAX_REQUEST_HEAD_BYTES),
            ));
        }
        let n = tokio::io::AsyncReadExt::read_buf(stream, &mut buf).await?;
        if n == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "connection closed before the request head was complete",
            ));
        }
    }
}

/// Offset of the first `\r\n\r\n` in `buf`, if present.
pub(super) fn find_head_end(buf: &[u8]) -> Option<usize> {
    if buf.len() < 4 {
        return None;
    }
    buf.windows(4).position(|w| w == b"\r\n\r\n")
}

/// Validate the upgrade request and return the client's
/// `Sec-WebSocket-Key`. Strictness matches the module docs: method GET,
/// HTTP/1.1, `Upgrade: websocket`, `Connection: upgrade`, a sane key, and
/// version 13 *if the header is sent at all*. Any path ("/") is accepted —
/// path-based routing is not this transport's job.
pub(super) fn parse_upgrade_request(head: &str) -> Result<String, String> {
    let mut lines = head.split("\r\n");
    let request_line = lines.next().unwrap_or("");
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("");
    let _path = parts.next().unwrap_or("");
    let version = parts.next().unwrap_or("");
    if method != "GET" {
        return Err(format!("expected GET, got `{method}`"));
    }
    if version != "HTTP/1.1" {
        return Err(format!("expected HTTP/1.1, got `{version}`"));
    }

    let mut upgrade: Option<&str> = None;
    let mut connection: Option<&str> = None;
    let mut key: Option<&str> = None;
    let mut ws_version: Option<&str> = None;
    for line in lines {
        if line.is_empty() {
            continue;
        }
        let Some((name, value)) = line.split_once(':') else {
            return Err(format!("malformed header line `{line}`"));
        };
        match name.trim().to_ascii_lowercase().as_str() {
            "upgrade" => upgrade = Some(value.trim()),
            "connection" => connection = Some(value.trim()),
            "sec-websocket-key" => key = Some(value.trim()),
            "sec-websocket-version" => ws_version = Some(value.trim()),
            _ => {}
        }
    }

    let Some(upgrade) = upgrade else {
        return Err("missing Upgrade header".into());
    };
    if !upgrade.eq_ignore_ascii_case("websocket") {
        return Err(format!("Upgrade is `{upgrade}`, want websocket"));
    }
    let Some(connection) = connection else {
        return Err("missing Connection header".into());
    };
    let upgrades_conn = connection
        .split(',')
        .any(|token| token.trim().eq_ignore_ascii_case("upgrade"));
    if !upgrades_conn {
        return Err(format!(
            "Connection is `{connection}`, want the upgrade token"
        ));
    }
    let Some(key) = key else {
        return Err("missing Sec-WebSocket-Key".into());
    };
    if key.is_empty() || key.len() > 128 || !key.bytes().all(|b| b.is_ascii_graphic()) {
        return Err("Sec-WebSocket-Key is empty or malformed".into());
    }
    if let Some(v) = ws_version.filter(|v| v.trim() != "13") {
        return Err(format!("unsupported Sec-WebSocket-Version `{v}`, want 13"));
    }
    Ok(key.to_owned())
}

#[cfg(test)]
mod tests;
