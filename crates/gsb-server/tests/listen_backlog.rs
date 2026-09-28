//! `listen_backlog` (BACKLOG B84): the server-level accept backlog of
//! every TCP-based listening socket — the plain TCP, TLS and WebSocket
//! doors and the ops HTTP surface. Omitted, it is the queue tokio's own
//! bind gave every door before the key existed; out of range, it refuses
//! startup before anything binds. That the value reaches each socket is
//! locked in `gsb_net::listen` and at the server's bind (`boot::accept`,
//! `boot::start::ops_door`).

use gsb_server::{Config, ListenerEntry, ListenerTransport, ServerError};

mod common;

/// Parse `text` as a config file's body.
fn parse(text: &str) -> Config {
    toml::from_str(text).expect("the config parses")
}

/// Omitted: the default every door had — tokio's (mio's) 128.
#[test]
fn omitted_it_is_the_queue_every_door_had() {
    assert_eq!(parse("").listen_backlog, 128);
    assert_eq!(
        Config::default().listen_backlog,
        gsb_net::listen::DEFAULT_LISTEN_BACKLOG
    );
    assert_eq!(parse("listen_backlog = 4096").listen_backlog, 4096);
}

/// A key the file's type cannot hold is a parse error, not a clamp.
#[test]
fn a_negative_value_does_not_parse() {
    let err = toml::from_str::<Config>("listen_backlog = -1").expect_err("refused");
    assert!(err.to_string().contains("listen_backlog"), "{err}");
}

/// A server-level key: `[rooms.<id>]` refuses it like every server-wide
/// key.
#[test]
fn a_room_override_cannot_carry_it() {
    let err = toml::from_str::<Config>("[rooms.1]\nlisten_backlog = 4096").expect_err("refused");
    assert!(err.to_string().contains("listen_backlog"), "{err}");
}

/// Zero and a value past a C `int` refuse startup, naming the key and the
/// kernel's cap — on a concrete port that stays free (nothing bound).
#[tokio::test]
async fn out_of_range_refuses_startup_before_anything_binds() {
    let free = std::net::TcpListener::bind("127.0.0.1:0").expect("probe");
    let port = free.local_addr().unwrap().port();
    drop(free);
    for bad in [0u32, 2_147_483_648, u32::MAX] {
        let cfg = Config {
            bind: format!("127.0.0.1:{port}"),
            listen_backlog: bad,
            ..Default::default()
        };
        let err = match gsb_server::start_server(cfg).await {
            Ok(_) => panic!("{bad} must refuse startup"),
            Err(e) => e,
        };
        assert!(
            matches!(err, ServerError::BadListenBacklog(n) if n == bad),
            "{err:?}"
        );
        let text = err.to_string();
        assert!(
            text.contains("listen_backlog") && text.contains("somaxconn"),
            "{text}"
        );
        std::net::TcpListener::bind(("127.0.0.1", port)).expect("the port was never taken");
    }
}

/// A large backlog (past this machine's `somaxconn`, which only caps it)
/// on every TCP-based door at once: the server starts, and each socket
/// takes a connection.
#[tokio::test]
async fn every_tcp_door_starts_with_a_large_backlog() {
    let pki = common::mint_tls_pki("backlog");
    let entry = |transport, tls: bool| ListenerEntry {
        transport,
        bind: "127.0.0.1:0".into(),
        tls_cert: tls.then(|| pki.cert_pem_path.clone()),
        tls_key: tls.then(|| pki.key_pem_path.clone()),
    };
    let cfg = Config {
        listeners: Some(vec![
            entry(ListenerTransport::Tcp, false),
            entry(ListenerTransport::Tls, true),
            entry(ListenerTransport::Ws, false),
        ]),
        http_listen: "127.0.0.1:0".into(),
        listen_backlog: 1 << 20,
        ..Default::default()
    };
    let handle = gsb_server::start_server(cfg).await.expect("server starts");
    let ops = handle.http_addr.expect("ops door");
    for addr in handle.addrs.iter().copied().chain([ops]) {
        tokio::net::TcpStream::connect(addr)
            .await
            .expect("connects");
    }
    handle.stop().await;
}
