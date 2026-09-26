//! The opening handshake: the request the door requires, the accept key
//! (the RFC vector), a random key per connection, bytes behind the 101
//! kept, and every answer that must fail it.

use std::io::{self, ErrorKind};

use super::*;

/// RFC 6455 §1.3's worked example.
#[test]
fn the_accept_key_matches_the_rfc_vector() {
    assert_eq!(
        accept_key("dGhlIHNhbXBsZSBub25jZQ=="),
        "s3pPLMBiTxaQ9kYGzzhZRbK+xOo="
    );
}

/// The request's head, and the value of one of its headers.
async fn request_head(io: &mut DuplexStream) -> String {
    let mut head = Vec::new();
    while !head.ends_with(b"\r\n\r\n") {
        head.push(io.read_u8().await.unwrap());
    }
    String::from_utf8(head).unwrap()
}

fn header<'a>(head: &'a str, name: &str) -> Option<&'a str> {
    head.lines().find_map(|l| {
        let (n, v) = l.split_once(':')?;
        n.trim().eq_ignore_ascii_case(name).then(|| v.trim())
    })
}

/// A scripted server: reads the request, hands its head to `answer`,
/// writes what that returns and hangs up. Returns the client's result
/// and the request head.
async fn against(
    answer: impl FnOnce(&str) -> Vec<u8> + Send + 'static,
) -> (io::Result<Conn>, String) {
    let (client, mut server) = duplex(1 << 16);
    let script = tokio::spawn(async move {
        let head = request_head(&mut server).await;
        server.write_all(&answer(&head)).await.unwrap();
        head
    });
    let conn = handshake(client, "gsb.test:7000", "/gsb").await;
    (conn, script.await.unwrap())
}

fn switching(head: &str, extra: &str) -> Vec<u8> {
    let key = header(head, "sec-websocket-key").unwrap();
    format!(
        "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\n\
         Connection: Upgrade\r\nSec-WebSocket-Accept: {}\r\n{extra}\r\n",
        accept_key(key)
    )
    .into_bytes()
}

/// The request carries what the door requires (GET, HTTP/1.1, Upgrade,
/// Connection, a 16-byte key, version 13, Host); a frame the server sent
/// in the same write as its 101 is the first frame read.
#[tokio::test]
async fn the_handshake_speaks_the_door_and_keeps_bytes_behind_the_101() {
    let (conn, head) = against(|head| {
        let mut out = switching(head, "");
        out.extend(server_frame(true, OP_BIN, &encode(42, b"early")));
        out
    })
    .await;
    assert!(head.starts_with("GET /gsb HTTP/1.1\r\n"), "{head}");
    assert_eq!(header(&head, "host"), Some("gsb.test:7000"));
    assert_eq!(header(&head, "upgrade"), Some("websocket"));
    assert_eq!(header(&head, "connection"), Some("Upgrade"));
    assert_eq!(header(&head, "sec-websocket-version"), Some("13"));
    let key = header(&head, "sec-websocket-key").unwrap();
    use base64::Engine;
    let nonce = base64::engine::general_purpose::STANDARD
        .decode(key)
        .unwrap();
    assert_eq!(nonce.len(), 16, "a 16-byte nonce");
    let mut conn = conn.expect("upgraded");
    assert!(conn.is_ws());
    let f = frame(&mut conn).await;
    assert_eq!((f.op, &f.payload[..]), (42, &b"early"[..]));
}

/// Every connection draws a fresh key (RFC 6455 §4.1: randomly chosen).
#[tokio::test]
async fn every_handshake_draws_a_fresh_key() {
    let mut keys = std::collections::HashSet::new();
    for _ in 0..4 {
        let (conn, head) = against(|head| switching(head, "")).await;
        conn.expect("upgraded");
        keys.insert(header(&head, "sec-websocket-key").unwrap().to_owned());
    }
    assert_eq!(keys.len(), 4, "{keys:?}");
}

/// A refusal, a wrong or missing accept key, a missing Upgrade or
/// Connection token, and a subprotocol or extension nobody asked for
/// each fail the handshake (`InvalidData`); a server that hangs up
/// before answering is `UnexpectedEof`.
#[tokio::test]
async fn every_bad_answer_fails_the_handshake() {
    type Answer = fn(&str) -> Vec<u8>;
    let bad: [(&str, Answer); 7] = [
        ("400", |_| b"HTTP/1.1 400 Bad Request\r\n\r\n".to_vec()),
        ("wrong accept", |_| {
            b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\n\
              Connection: Upgrade\r\nSec-WebSocket-Accept: s3pPLMBiTxaQ9kYGzzhZRbK+xOo=\r\n\r\n"
                .to_vec()
        }),
        ("no accept", |_| {
            b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\n\
              Connection: Upgrade\r\n\r\n"
                .to_vec()
        }),
        ("no upgrade", |h| {
            let s = String::from_utf8(switching(h, "")).unwrap();
            s.replace("Upgrade: websocket\r\n", "").into_bytes()
        }),
        ("no connection", |h| {
            let s = String::from_utf8(switching(h, "")).unwrap();
            s.replace("Connection: Upgrade\r\n", "").into_bytes()
        }),
        ("subprotocol", |h| {
            switching(h, "Sec-WebSocket-Protocol: chat\r\n")
        }),
        ("extension", |h| {
            switching(h, "Sec-WebSocket-Extensions: permessage-deflate\r\n")
        }),
    ];
    for (what, answer) in bad {
        let (conn, _) = against(answer).await;
        let e = conn.err().unwrap_or_else(|| panic!("{what}: must fail"));
        assert_eq!(e.kind(), ErrorKind::InvalidData, "{what}: {e}");
    }
    let (conn, _) = against(|_| Vec::new()).await;
    assert_eq!(
        conn.err().expect("no answer").kind(),
        ErrorKind::UnexpectedEof
    );
}
