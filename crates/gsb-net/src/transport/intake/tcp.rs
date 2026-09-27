//! The intake task of a door over TCP (WebSocket, TLS): accept raw
//! sockets, and give each a slot and a handshake task — or close it.

use std::io;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use tokio::net::{TcpListener, TcpStream};
use tracing::{debug, warn};

use crate::transport::intake::Intake;
use crate::transport::{Endpoint, is_listener_closed};

/// The pause after a raw accept error (e.g. EMFILE), so a persistent one
/// does not spin: the server accept loop's own back-off, moved here with
/// the raw accept.
const ACCEPT_BACKOFF: Duration = Duration::from_millis(100);

/// Run a door's intake until its door closes: every raw socket either
/// gets a slot and a handshake task (`handshake(stream, peer)` under
/// `deadline`), or is closed on the spot. The listener socket is dropped
/// — the port released — when the loop ends.
pub(crate) async fn run_tcp_intake<H, F>(
    intake: Arc<Intake>,
    listener: TcpListener,
    deadline: Duration,
    handshake: H,
    metrics: crate::TransportMetrics,
) where
    H: Fn(TcpStream, SocketAddr) -> F,
    F: Future<Output = io::Result<Endpoint>> + Send + 'static,
{
    let mut flusher = crate::metrics::Flusher::new(metrics);
    loop {
        match intake.door().admit(listener.accept()).await {
            Ok((stream, peer)) => match intake.try_slot() {
                Some(slot) => intake.spawn(slot, peer, deadline, handshake(stream, peer)),
                // Refused: the socket closes as it drops, unhandshaken.
                None => debug!(%peer, "handshake bound reached; connection refused"),
            },
            Err(e) if is_listener_closed(&e) => break,
            Err(e) => {
                warn!(%e, "accept error; backing off");
                let pause = async {
                    tokio::time::sleep(ACCEPT_BACKOFF).await;
                    Ok(())
                };
                // A close during the pause is seen by the next accept.
                let _ = intake.door().admit(pause).await;
            }
        }
        intake.flush_metrics(&mut flusher, false);
    }
    // The port goes now; the last sample waits for the close's counts.
    drop(listener);
    intake.settle().await;
    intake.flush_metrics(&mut flusher, true);
    intake.log_summary();
}
