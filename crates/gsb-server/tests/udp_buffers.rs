//! `udp_recv_buffer_bytes` / `udp_send_buffer_bytes` (BACKLOG B4): the
//! server-level kernel buffers of every UDP-based door's socket — the
//! rUDP and QUIC doors. Omitted, nothing is touched (the system
//! default); out of range, startup is refused before anything binds.
//! That the value reaches each socket is locked in `gsb_net::listen`, in
//! each door's own tests, and at the server's bind (`boot::accept`).

use gsb_server::{Config, ListenerEntry, ListenerTransport, ServerError};

mod common;

/// Parse `text` as a config file's body.
fn parse(text: &str) -> Config {
    toml::from_str(text).expect("the config parses")
}

/// Omitted: untouched (no `setsockopt`); written: the value.
#[test]
fn omitted_they_leave_the_system_default() {
    let cfg = parse("");
    assert_eq!(cfg.udp_recv_buffer_bytes, None);
    assert_eq!(cfg.udp_send_buffer_bytes, None);
    assert_eq!(Config::default().udp_recv_buffer_bytes, None);
    let cfg = parse("udp_recv_buffer_bytes = 4194304\nudp_send_buffer_bytes = 1048576");
    assert_eq!(cfg.udp_recv_buffer_bytes, Some(4_194_304));
    assert_eq!(cfg.udp_send_buffer_bytes, Some(1_048_576));
}

/// A value the file's type cannot hold is a parse error, not a clamp;
/// and they are server-level keys, which `[rooms.<id>]` refuses.
#[test]
fn a_negative_value_or_a_room_override_does_not_parse() {
    for text in [
        "udp_recv_buffer_bytes = -1",
        "udp_send_buffer_bytes = -1",
        "[rooms.1]\nudp_recv_buffer_bytes = 65536",
    ] {
        let err = toml::from_str::<Config>(text).expect_err("refused");
        assert!(err.to_string().contains("_buffer_bytes"), "{text}: {err}");
    }
}

/// Below a page or past a C `int`, either key refuses startup, naming
/// the key and the kernel's cap — on a concrete port that stays free.
#[tokio::test]
async fn out_of_range_refuses_startup_before_anything_binds() {
    let free = std::net::UdpSocket::bind("127.0.0.1:0").expect("probe");
    let port = free.local_addr().unwrap().port();
    drop(free);
    for bad in [0u32, 4095, 2_147_483_648, u32::MAX] {
        for recv in [true, false] {
            let cfg = Config {
                bind: format!("127.0.0.1:{port}"),
                transport: gsb_server::TransportKind::Udp,
                udp_recv_buffer_bytes: recv.then_some(bad),
                udp_send_buffer_bytes: (!recv).then_some(bad),
                udp_static_key: Some(common::rudp_key().0.clone()),
                ..Default::default()
            };
            let err = match gsb_server::start_server(cfg).await {
                Ok(_) => panic!("{bad} must refuse startup"),
                Err(e) => e,
            };
            let key = if recv {
                "udp_recv_buffer_bytes"
            } else {
                "udp_send_buffer_bytes"
            };
            assert!(
                matches!(err, ServerError::BadUdpBuffer { key: k, value } if k == key && value == bad),
                "{err:?}"
            );
            let text = err.to_string();
            assert!(text.contains(key) && text.contains("rmem_max"), "{text}");
            std::net::UdpSocket::bind(("127.0.0.1", port)).expect("the port was never taken");
        }
    }
}

/// Both UDP doors at once, with buffers inside the range (the receive
/// one past this machine's cap, which only caps it): the server starts
/// and each door answers — the rUDP door its handshake, the QUIC door a
/// connection.
#[tokio::test]
async fn every_udp_door_starts_with_its_buffers() {
    let pki = common::mint_tls_pki("udp-buffers");
    let entry = |transport, tls: bool| ListenerEntry {
        transport,
        bind: "127.0.0.1:0".into(),
        tls_cert: tls.then(|| pki.cert_pem_path.clone()),
        tls_key: tls.then(|| pki.key_pem_path.clone()),
    };
    let cfg = Config {
        listeners: Some(vec![
            entry(ListenerTransport::Udp, false),
            entry(ListenerTransport::Quic, true),
        ]),
        udp_recv_buffer_bytes: Some(1 << 30),
        udp_send_buffer_bytes: Some(1 << 20),
        udp_static_key: Some(common::rudp_key().0.clone()),
        ..Default::default()
    };
    let handle = gsb_server::start_server(cfg).await.expect("server starts");
    let pinned = gsb_net::udp::UdpClientConfig {
        server_key: common::rudp_pin(),
        ..Default::default()
    };
    let udp = gsb_net::udp::UdpClient::connect_with(handle.addrs[0], pinned)
        .await
        .expect("the rUDP door handshakes");
    assert!(udp.is_established());
    let config = gsb_client::quic::client_config([pki.ca_der.clone()]).expect("QUIC TLS");
    let quic = gsb_client::quic::connect(handle.addrs[1], common::TLS_SERVER_NAME, config)
        .await
        .expect("the QUIC door handshakes");
    drop(quic);
    handle.stop().await;
}
