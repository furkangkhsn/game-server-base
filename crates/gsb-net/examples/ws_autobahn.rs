//! RFC 6455 conformance harness: a WebSocket-only echo server for the
//! Autobahn TestSuite fuzzing client (the CI `autobahn` job; the case
//! selection and the by-contract exclusions live in `.github/autobahn/`).
//!
//! ```text
//! cargo run --release -p gsb-net --example ws_autobahn -- [ADDR] [MAX_MESSAGE_BYTES]
//! ```
//!
//! ADDR defaults to 127.0.0.1:9001, MAX_MESSAGE_BYTES to 16 MiB (the
//! largest Autobahn binary message; the server door's own ceiling is the
//! frame cap plus 4).
//!
//! It runs the production WS door — handshake, frame parser, reassembly,
//! control frames, close handshake, reader/writer pumps — with ONE
//! difference: the opaque message mapping ([`WsMessageMapping::Opaque`]).
//! Autobahn verifies echoes of arbitrary binary payloads, which the game
//! envelope would refuse with 1007, so each binary message is echoed as
//! it arrived. Text is still refused with 1003, exactly as on a server
//! door: that is the wire contract, not something to test around.
//!
//! One echo task per connection is fine here: this is a test fixture
//! serving one Autobahn case at a time, not the engine.

use std::io;
use std::net::SocketAddr;
use std::sync::Arc;

use gsb_core::channel::FrameBatch;
use gsb_core::channel::channel;
use gsb_core::conn::ConnIn;
use gsb_core::id::ConnectionId;
use gsb_net::Transport;
use gsb_net::pump::PumpTimeouts;
use gsb_net::ws::WsMessageMapping;
use gsb_net::ws::WsTransport;

const DEFAULT_ADDR: &str = "127.0.0.1:9001";
const DEFAULT_MAX_MESSAGE_BYTES: usize = 16 * 1024 * 1024;

#[tokio::main]
async fn main() -> io::Result<()> {
    let mut args = std::env::args().skip(1);
    let addr: SocketAddr = args
        .next()
        .unwrap_or_else(|| DEFAULT_ADDR.to_owned())
        .parse()
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, format!("ADDR: {e}")))?;
    let max_message_bytes = match args.next() {
        Some(raw) => raw.parse().map_err(|e| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("MAX_MESSAGE_BYTES: {e}"),
            )
        })?,
        None => DEFAULT_MAX_MESSAGE_BYTES,
    };

    let transport = Arc::new(WsTransport {
        max_message_bytes,
        mapping: WsMessageMapping::Opaque,
        ..WsTransport::default()
    });
    let listener = transport.bind(addr).await?;
    let bound = listener.local_addr().unwrap_or(addr);
    println!("ws_autobahn: echoing on ws://{bound} (max message {max_message_bytes} bytes)");

    let mut next_id = 0u64;
    loop {
        // A failed or timed-out upgrade is one bad client: the door
        // counts it and never hands it here (BACKLOG B31), so an accept
        // error is the door itself closing.
        let endpoint = match Arc::clone(&listener).accept().await {
            Ok(endpoint) => endpoint,
            Err(e) => {
                eprintln!("ws_autobahn: accept failed: {e}");
                return Err(e);
            }
        };
        next_id += 1;
        let (in_tx, mut in_rx) = channel::<ConnIn>(64);
        let (out_tx, out_rx) = channel::<FrameBatch>(64);
        let (read, write) = endpoint.start_pump(
            ConnectionId(next_id),
            in_tx,
            out_rx,
            PumpTimeouts::default(),
        );
        tokio::spawn(async move {
            while let Some(msg) = in_rx.recv().await {
                let open = match msg {
                    // Echo; a gone writer side ends the connection.
                    ConnIn::Frame(frame) => out_tx.send(vec![frame]).await.is_ok(),
                    // The peer's close, a protocol failure, or any other
                    // end the reader pump reports: the connection is over.
                    ConnIn::Closed { .. }
                    | ConnIn::StreamRejected { .. }
                    | ConnIn::ServerClosed { .. } => false,
                    _ => true,
                };
                if !open {
                    break;
                }
            }
            drop(out_tx);
            if let Some(read) = read {
                let _ = read.await;
            }
            let _ = write.await;
        });
    }
}
