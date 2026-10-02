//! The ops HTTP surface's two limits (BACKLOG B49) behind the composition
//! root: `http_max_connections` and `http_write_timeout_secs` — their
//! defaults and parsing, and a connection over the cap refused at once
//! and counted in the surface's own `/metrics`.

use std::net::SocketAddr;
use std::time::Duration;

use gsb_server::Config;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// Far below the 5 s head deadline that frees a silent holder's slot.
const PROMPT: Duration = Duration::from_secs(2);

fn parse(text: &str) -> Config {
    toml::from_str(text).expect("the config parses")
}

/// On by default, with generous values; both settable; `0` is spelled
/// in the file and resolved at use (no cap, no deadline).
#[test]
fn defaults_and_parsing() {
    let d = parse("");
    assert_eq!(d.http_max_connections, Some(64));
    assert_eq!(d.http_write_timeout_secs, 10.0);
    let c = parse("http_max_connections = 8\nhttp_write_timeout_secs = 2.5");
    assert_eq!(c.http_max_connections, Some(8));
    assert_eq!(c.http_write_timeout_secs, 2.5);
    assert_eq!(
        parse("http_max_connections = 0").http_max_connections,
        Some(0)
    );
}

/// Server-level keys: a negative cap does not parse; a room override
/// cannot carry either.
#[test]
fn they_are_server_level_keys() {
    let err = toml::from_str::<Config>("http_max_connections = -1").expect_err("refused");
    assert!(err.to_string().contains("http_max_connections"), "{err}");
    for key in ["http_max_connections = 2", "http_write_timeout_secs = 1.0"] {
        let err = toml::from_str::<Config>(&format!("[rooms.1]\n{key}")).expect_err("refused");
        let name = key.split(' ').next().unwrap();
        assert!(err.to_string().contains(name), "{err}");
    }
}

/// One request; the whole answer — empty when the surface refused the
/// connection at its cap (closed unanswered, or reset).
async fn get(addr: SocketAddr, path: &str) -> String {
    let mut s = TcpStream::connect(addr).await.expect("connect");
    let mut out = Vec::new();
    if s.write_all(format!("GET {path} HTTP/1.1\r\nHost: x\r\n\r\n").as_bytes())
        .await
        .is_ok()
    {
        let _ = tokio::time::timeout(PROMPT, s.read_to_end(&mut out))
            .await
            .expect("answered or closed");
    }
    String::from_utf8(out).expect("utf-8")
}

#[tokio::test]
async fn over_the_cap_a_connection_is_refused_and_counted() {
    let cfg = Config {
        room_count: 1,
        bind: "127.0.0.1:0".into(),
        http_listen: "127.0.0.1:0".into(),
        http_max_connections: Some(2),
        ..Default::default()
    };
    let handle = gsb_server::start_server(cfg).await.expect("server starts");
    let addr = handle.http_addr.expect("the ops surface");

    // Two silent holders take both slots; the third is closed at once,
    // unanswered.
    let first = TcpStream::connect(addr).await.expect("connect");
    let second = TcpStream::connect(addr).await.expect("connect");
    tokio::time::sleep(Duration::from_millis(50)).await;
    let mut third = TcpStream::connect(addr).await.expect("connect");
    let mut byte = [0u8; 1];
    let read = tokio::time::timeout(PROMPT, third.read(&mut byte))
        .await
        .expect("refused at once, not held to the head deadline");
    assert!(matches!(read, Ok(0)) || read.is_err(), "{read:?}");
    drop((first, second));

    // The refusal reaches the surface's own exposition (after the accept
    // loop's next flush and the collector's next report).
    // A poll that meets the holders' tasks still ending is refused too:
    // counted here as well, so the expected value stays exact.
    if cfg!(feature = "prometheus") {
        let mut refused = 1;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            let text = get(addr, "/metrics").await;
            if text.is_empty() {
                refused += 1;
            } else if text.contains(&format!(
                "\ngsb_transport_ops_http_conns_refused_total {refused}\n"
            )) {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "never counted:\n{text}"
            );
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }
    handle.stop().await;
}
