//! Starting the server child and learning where it listens (BACKLOG
//! F31). The child binds port 0 for its game door and its metric stream
//! and reports the bound addresses on its `SERVING` line
//! (`serve::announce`); nothing here picks a port. It used to: bind port
//! 0, read the number, close, pass it on — and a port taken by anyone in
//! between killed the child with "Address already in use".

use super::*;
use gsb_net::udp::UdpClient;
use std::io::Write;
use std::process::ExitStatus;
use tokio::io::{AsyncBufRead, AsyncWriteExt};
use tokio::net::UdpSocket;
use tokio::task::JoinHandle;

/// How long the server child may take to report its addresses. A
/// start-up takes milliseconds; the bound only turns a child that
/// neither reports nor exits into an error instead of a hang (a frozen,
/// swapping machine still gets a generous margin).
pub(crate) const SERVER_REPORT_BOUND: Duration = Duration::from_secs(30);

/// Why the child's stdout gave no usable `SERVING` line.
#[derive(Debug)]
pub(crate) enum ReportError {
    /// Stdout ended first: the child exited (its status and its own
    /// stderr say why — a refused config, a refused bind).
    Exited,
    /// Neither the line nor the end of stdout within the bound.
    Silent(Duration),
    /// A line that starts like the report but does not parse, or names
    /// no metric stream (the orchestrator always asks for one).
    Malformed(String),
    /// Reading the pipe failed.
    Read(std::io::Error),
}

impl std::fmt::Display for ReportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Exited => f.write_str("exited before reporting its addresses"),
            Self::Silent(b) => write!(f, "reported no addresses within {} s", b.as_secs_f64()),
            Self::Malformed(line) => write!(f, "reported an unusable line: {line:?}"),
            Self::Read(e) => write!(f, "could not be read: {e}"),
        }
    }
}

/// Read the child's stdout up to its `SERVING` line, within `bound`.
/// Lines before it go to `forward` (the child's log lines — its tracing
/// writes to stdout, which was this process's stdout before the pipe).
pub(crate) async fn await_serving<R: AsyncBufRead + Unpin>(
    reader: &mut R,
    bound: Duration,
    mut forward: impl FnMut(&[u8]),
) -> Result<(SocketAddr, SocketAddr, Option<[u8; 32]>), ReportError> {
    let read = async {
        let mut line = Vec::new();
        loop {
            line.clear();
            match reader.read_until(b'\n', &mut line).await {
                Ok(0) => return Err(ReportError::Exited),
                Ok(_) => {}
                Err(e) => return Err(ReportError::Read(e)),
            }
            let text = String::from_utf8_lossy(&line);
            if !text.starts_with(SERVING_PREFIX) {
                forward(&line);
                continue;
            }
            return match Serving::parse(&text) {
                Some(Serving {
                    addr,
                    metrics: Some(metrics),
                    udp_key,
                }) => Ok((addr, metrics, udp_key)),
                _ => Err(ReportError::Malformed(text.trim_end().to_string())),
            };
        }
    };
    tokio::time::timeout(bound, read)
        .await
        .unwrap_or(Err(ReportError::Silent(bound)))
}

/// Pass the rest of the child's stdout on to this process's stdout, line
/// by line (bytes as they are), until the child closes it at its exit.
pub(crate) async fn forward_stdout<R: AsyncBufRead + Unpin>(mut reader: R) {
    let mut line = Vec::new();
    loop {
        line.clear();
        match reader.read_until(b'\n', &mut line).await {
            Ok(0) | Err(_) => break,
            Ok(_) => print_raw(&line),
        }
    }
}

/// One line of the child's stdout, onto ours.
fn print_raw(line: &[u8]) {
    let _ = std::io::stdout().lock().write_all(line);
}

/// A running server child whose addresses are known.
pub(crate) struct ServerChild {
    pub(crate) child: Child,
    pub(crate) pid: u32,
    /// The game door the clients are aimed at (as reported).
    pub(crate) addr: SocketAddr,
    /// The metric stream the orchestrator reads (as reported).
    pub(crate) metrics: SocketAddr,
    /// The sealed rUDP door's public key (as reported; B5a).
    pub(crate) udp_key: Option<[u8; 32]>,
    /// Passes the child's later stdout on; ends at the child's exit.
    pub(crate) stdout_forward: JoinHandle<()>,
}

/// Spawn the server child (`argv`, pinned to `mask` like every child) and
/// wait — at most `bound` — for its `SERVING` line. On failure the child
/// is reaped (killed first when it is still running) and the error names
/// what happened, with its exit status.
pub(crate) async fn start_server_child(
    exe: &std::path::Path,
    argv: &[String],
    mask: &Option<Vec<u32>>,
    taskset: Option<&std::path::Path>,
    bound: Duration,
) -> Result<ServerChild, String> {
    let mut child = spawn_pinned(exe, argv, &[], mask, taskset, "server", true)
        .await
        .map_err(|e| format!("the server child did not spawn: {e}"))?;
    let pid = child.id().expect("freshly spawned child has a pid");
    let mut reader = BufReader::new(child.stdout.take().expect("stdout piped"));
    match await_serving(&mut reader, bound, print_raw).await {
        Ok((addr, metrics, udp_key)) => Ok(ServerChild {
            child,
            pid,
            addr,
            metrics,
            udp_key,
            stdout_forward: tokio::spawn(forward_stdout(reader)),
        }),
        Err(why) => {
            if !matches!(why, ReportError::Exited) {
                let _ = child.kill().await;
            }
            let status = child.wait().await;
            Err(format!(
                "the server child {why} ({})",
                describe_exit(status)
            ))
        }
    }
}

/// A reaped child's exit, for the error line.
fn describe_exit(status: std::io::Result<ExitStatus>) -> String {
    match status {
        Ok(s) => s.to_string(),
        Err(e) => format!("its exit status is unknown: {e}"),
    }
}

/// Wait until the reported game door answers, BEFORE the client children
/// start: a TCP connect (a kernel-backlog handshake counts), or on rUDP —
/// no SYN to probe with — one cookie-handshake CHALLENGE (a bare challenge
/// request establishes nothing on the server). The door is bound before
/// the report, so this normally passes at once; it only makes sure the
/// clients, which never retry, meet a server that answers, so their
/// connect_ms stays a pure measurement. The probe is one clean open/close
/// on the server (no frames, no room).
pub(crate) async fn probe_door(transport: crate::Transport, addr: SocketAddr) {
    let deadline = Instant::now() + Duration::from_secs(10);
    let probe_sock = UdpSocket::bind("0.0.0.0:0".parse::<SocketAddr>().unwrap())
        .await
        .expect("probe socket binds");
    loop {
        let ready = if transport == crate::Transport::Udp {
            UdpClient::challenge_probe(&probe_sock, addr, Duration::from_millis(200)).await
        } else {
            match TcpStream::connect(addr).await {
                Ok(mut s) => {
                    let _ = s.shutdown().await;
                    true
                }
                Err(_) => false,
            }
        };
        if ready {
            return;
        }
        if Instant::now() >= deadline {
            eprintln!(
                "orchestrate: server socket not ready after 10 s; clients will report their own connect failures"
            );
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[cfg(test)]
mod tests;
