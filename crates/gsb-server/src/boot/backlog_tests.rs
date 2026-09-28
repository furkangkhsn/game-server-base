//! `listen_backlog` reaches every TCP-based socket the server binds
//! (BACKLOG B84): the doors `bind_listener` builds and the ops surface.
//! Where nobody accepts (a plain TCP door, the ops socket before its
//! task runs) the queue itself is observed; the doors whose intake
//! accepts eagerly (TLS, WebSocket) show the value reaching the socket
//! builder by its refusal of a zero — which startup refuses first, so
//! only this crate-internal path can hand one down.

use std::net::SocketAddr;
use std::time::Duration;

use tokio::net::TcpStream;

use super::accept::bind_listener;
use super::start::ops_door::bind_ops;
use crate::config::*;

/// A config binding the ops surface on an ephemeral port, with `backlog`.
fn config(backlog: u32) -> Config {
    Config {
        http_listen: "127.0.0.1:0".into(),
        listen_backlog: backlog,
        ..Default::default()
    }
}

fn any_port() -> SocketAddr {
    "127.0.0.1:0".parse().unwrap()
}

/// How many of `tries` connects to `addr`, one after another while
/// nobody accepts, complete within 300 ms each — stopping at the first
/// whose SYN the full queue dropped (see `gsb_net::listen`'s tests).
#[cfg(target_os = "linux")]
async fn queued(addr: SocketAddr, tries: usize) -> usize {
    let mut held = Vec::with_capacity(tries);
    for _ in 0..tries {
        match tokio::time::timeout(Duration::from_millis(300), TcpStream::connect(addr)).await {
            Ok(Ok(stream)) => held.push(stream),
            _ => break,
        }
    }
    held.len()
}

/// Bind one door of `spec` under `cfg` (no idle window, no cookie key,
/// the default handshake bound, no metrics).
async fn door(
    spec: &ListenerSpec,
    cfg: &Config,
) -> Result<(std::sync::Arc<dyn gsb_net::Listener>, SocketAddr), ServerError> {
    let bound = gsb_net::transport::DEFAULT_MAX_PENDING_HANDSHAKES;
    bind_listener(spec, cfg, None, None, bound, None).await
}

/// A backlog of one on the plain TCP door and on the ops socket: a
/// couple of connects queue, not sixteen (the default takes 128).
#[cfg(target_os = "linux")]
#[tokio::test]
async fn a_backlog_of_one_reaches_the_tcp_door_and_the_ops_socket() {
    let cfg = config(1);
    let (_door, addr) = door(&ListenerSpec::Tcp { addr: any_port() }, &cfg)
        .await
        .expect("bind");
    let n = queued(addr, 16).await;
    assert!((1..=4).contains(&n), "tcp door: {n} queued behind 1");
    let (_ops, addr) = bind_ops(&cfg).expect("bind");
    let n = queued(addr, 16).await;
    assert!((1..=4).contains(&n), "ops socket: {n} queued behind 1");
}

/// A zero, handed past startup's check, is refused by the builder on
/// every TCP-based door and on the ops socket.
#[tokio::test]
async fn every_tcp_socket_hands_its_backlog_to_the_builder() {
    let dir = std::env::temp_dir().join(format!("gsb-b84-backlog-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let key = rcgen::KeyPair::generate().expect("key");
    let params = rcgen::CertificateParams::new(vec!["localhost".into()]).expect("params");
    let cert = params.self_signed(&key).expect("cert");
    let (cert_pem, key_pem) = (dir.join("cert.pem"), dir.join("key.pem"));
    std::fs::write(&cert_pem, cert.pem()).expect("write cert");
    std::fs::write(&key_pem, key.serialize_pem()).expect("write key");

    let cfg = config(0);
    let specs = [
        ListenerSpec::Tcp { addr: any_port() },
        ListenerSpec::Tls {
            addr: any_port(),
            cert_pem: cert_pem.display().to_string(),
            key_pem: key_pem.display().to_string(),
        },
        ListenerSpec::Ws { addr: any_port() },
    ];
    for spec in &specs {
        match door(spec, &cfg).await {
            Err(ServerError::ListenerBind {
                addr,
                transport,
                source,
            }) => {
                assert_eq!(addr, spec.addr());
                assert_eq!(transport, spec.transport());
                assert_eq!(source.kind(), std::io::ErrorKind::InvalidInput, "{source}")
            }
            Err(e) => panic!("refused for another reason: {e}"),
            Ok(_) => panic!("a zero backlog bound a door"),
        }
    }
    match bind_ops(&cfg) {
        Err(ServerError::BadHttpListen(_, why)) => assert!(why.contains("backlog"), "{why}"),
        Err(e) => panic!("refused for another reason: {e}"),
        Ok(_) => panic!("a zero backlog bound the ops socket"),
    }
    let _ = std::fs::remove_dir_all(&dir);
}
