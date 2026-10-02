//! The write-stall clock on the TLS door counts the bytes the SOCKET
//! takes (BACKLOG B15a).
//!
//! rustls keeps up to 64 KiB of ciphertext of its own. Once the framing
//! writer has handed rustls a frame's last plaintext byte, that tail
//! drains into the socket inside `poll_flush`, which reports no byte
//! count: the clock counted plaintext handed to rustls, so a peer that
//! reads the tail slower than 64 KiB per window was cut off while it was
//! reading. The count now comes from beneath rustls (`crate::wire`).
//!
//! Forced on a real socket: both kernel buffers are shrunk, so the
//! peer's pace — not buffer headroom — drains rustls's tail, and the
//! tail alone takes several windows.

use super::*;

use std::io::Read;

use tokio::io::AsyncReadExt;
use tokio::net::TcpSocket;

use crate::pump::PumpTimeouts;

/// The stall window under test (the TCP door's slow-reader test's, for
/// the same reason: far from the peer's pace under any machine stall).
const WINDOW: Duration = Duration::from_secs(1);
/// One frame: bigger than rustls's 64 KiB buffer, so the tail it holds
/// at the flush is the full 64 KiB.
const FRAME: usize = 96 * 1024;
/// The peer's pace: at most this many raw bytes per read, one read per
/// tick — 64 KiB takes ≥ 2.5 s, i.e. ≥ 2 windows, on any machine.
const READ_CHUNK: usize = 1024;
const READ_EVERY: Duration = Duration::from_millis(40);

/// A handshaken TLS pair with both kernel buffers shrunk: the client's
/// receive buffer (set before connecting) and the server's send buffer
/// (which also switches autotuning off). The server side is the door's
/// own `handshake`, so its endpoint carries the door's byte count.
async fn tight_pair(
    pki: &TestPki,
) -> (
    tokio_rustls::client::TlsStream<tokio::net::TcpStream>,
    Endpoint,
) {
    let config = transport_for(pki).config;
    let acceptor = TlsAcceptor::from(Arc::new(load_server_config(&config).expect("identity")));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let csock = TcpSocket::new_v4().unwrap();
    csock.set_recv_buffer_size(4096).unwrap();
    let name: rustls::pki_types::ServerName<'static> = "localhost".try_into().expect("dns name");
    let client = async {
        let tcp = csock.connect(addr).await.expect("connect");
        client_connector(pki).connect(name, tcp).await.expect("tls")
    };
    let server = async {
        let (stream, peer) = listener.accept().await.expect("accept");
        socket2::SockRef::from(&stream)
            .set_send_buffer_size(4096)
            .unwrap();
        handshake(acceptor, stream, peer, FRAME * 2, None)
            .await
            .expect("handshake")
    };
    tokio::join!(client, server)
}

/// THE REGRESSION LOCK: a peer that reads the whole frame, slowly but
/// without a pause anywhere near the window, keeps its session.
#[tokio::test]
async fn a_slow_reader_survives_the_tail_rustls_holds() {
    let pki = mint_pki("slow-tail");
    let (client, endpoint) = tight_pair(&pki).await;

    let (in_tx, mut in_rx) = channel::<ConnIn>(8);
    let (out_tx, out_rx) = channel::<FrameBatch>(4);
    let (read, write) = endpoint.start_pump(
        ConnectionId(41),
        in_tx,
        out_rx,
        PumpTimeouts {
            idle: None,
            write_stall: Some(WINDOW),
        },
    );
    out_tx
        .send(vec![FrameBody::new(7, vec![0u8; FRAME])])
        .await
        .expect("the writer pump takes the frame");

    // Read the RAW socket at the pace above, decrypting by hand, until
    // the whole frame (length prefix + opcode + payload) is in.
    let (mut tcp, mut tls) = client.into_inner();
    let want = 4 + 2 + FRAME;
    let started = std::time::Instant::now();
    let mut got = 0usize;
    let mut raw = vec![0u8; READ_CHUNK];
    let mut plain = vec![0u8; 64 * 1024];
    while got < want {
        tokio::time::sleep(READ_EVERY).await;
        let n = match tokio::time::timeout(Duration::from_secs(5), tcp.read(&mut raw)).await {
            Ok(Ok(0)) | Ok(Err(_)) => panic!("the server closed after {got} of {want} bytes"),
            Ok(Ok(n)) => n,
            Err(_) => panic!(
                "the server stopped writing after {got} of {want} bytes ({:?} in): {:?}",
                started.elapsed(),
                in_rx.try_recv()
            ),
        };
        tls.read_tls(&mut &raw[..n]).expect("ciphertext");
        tls.process_new_packets().expect("records");
        loop {
            match tls.reader().read(&mut plain) {
                Ok(0) => break,
                Ok(k) => got += k,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) => panic!("plaintext: {e}"),
            }
        }
    }
    let took = started.elapsed();

    // Not vacuous: rustls's tail alone took more than a window.
    assert!(
        took >= WINDOW * 2,
        "the frame drained in {took:?}; it must take several {WINDOW:?} \
         windows for this test to mean anything"
    );
    assert!(
        in_rx.try_recv().is_err(),
        "no close may have been reported for a peer that kept reading"
    );

    drop(out_tx);
    tokio::time::timeout(Duration::from_secs(5), write)
        .await
        .expect("writer pump exits when the channel closes")
        .expect("no panic");
    if let Some(read) = read {
        read.abort();
    }
}

/// The converse, so the count beneath rustls cannot be "always moving":
/// a peer that never reads still ends the session on the window.
#[tokio::test]
async fn a_deaf_peer_still_stalls() {
    let pki = mint_pki("deaf-tail");
    let (_client, endpoint) = tight_pair(&pki).await;
    let (in_tx, mut in_rx) = channel::<ConnIn>(8);
    let (out_tx, out_rx) = channel::<FrameBatch>(4);
    let (read, write) = endpoint.start_pump(
        ConnectionId(42),
        in_tx,
        out_rx,
        PumpTimeouts {
            idle: None,
            write_stall: Some(Duration::from_millis(300)),
        },
    );
    out_tx
        .send(vec![FrameBody::new(7, vec![0u8; FRAME])])
        .await
        .expect("the writer pump takes the frame");
    let msg = tokio::time::timeout(Duration::from_secs(10), in_rx.recv())
        .await
        .expect("the stalled TLS socket was never reported")
        .expect("pump notified");
    assert!(
        matches!(
            msg,
            ConnIn::ServerClosed {
                cause: gsb_core::conn::ServerClose::WriteStall,
                ..
            }
        ),
        "{msg:?}"
    );
    tokio::time::timeout(Duration::from_secs(5), write)
        .await
        .expect("writer pump exits without the wedged socket")
        .expect("no panic");
    if let Some(read) = read {
        read.abort();
    }
}
