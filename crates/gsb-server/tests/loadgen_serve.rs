//! The served server (`gsb-loadgen --serve`) reports the addresses it
//! actually bound (BACKLOG F31). The orchestrator used to pick its
//! child's ports itself — bind port 0, read the number, close, pass it on
//! — and a port taken by anyone else between that close and the child's
//! bind killed the child with "Address already in use". The child now
//! binds port 0 and says which ports it got; this is that report, seen
//! from outside the process.
//!
//! And a served server nobody reads still ends (BACKLOG F40): its metric
//! export used to wait on `accept` forever when no one connected.

use std::io::{BufRead, BufReader, Read};
use std::net::{SocketAddr, TcpStream};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// How long the child may take to report — a start-up is milliseconds;
/// the bound only turns a silent child into a failure instead of a hang.
const REPORT_BOUND: Duration = Duration::from_secs(60);

/// The child, killed and reaped however the test ends — a failed
/// assertion must not leave a served server behind (before F40, with no
/// one on its metric stream it waited for a connection forever).
struct Reaped(Child);

impl Drop for Reaped {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// The value of `key=` in a `SERVING` line.
fn field<'a>(line: &'a str, key: &str) -> &'a str {
    line.split_whitespace()
        .find_map(|p| p.strip_prefix(key)?.strip_prefix('='))
        .unwrap_or_else(|| panic!("no {key}= in: {line}"))
}

/// A `--serve` child told to bind port 0 for both its game door and its
/// metric stream prints one `SERVING` line on stdout naming the real
/// ports; both accept a connection there, and the metric stream carries
/// the server's reports (at least the final one of the clean stop).
#[test]
fn a_served_server_reports_the_ports_it_bound() {
    let mut child = Reaped(
        Command::new(env!("CARGO_BIN_EXE_gsb-loadgen"))
            .args(["--serve", "--bind", "127.0.0.1:0"])
            .args(["--metrics-listen", "127.0.0.1:0", "--duration", "2"])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawning gsb-loadgen --serve"),
    );
    let stdout = child.0.stdout.take().expect("stdout piped");
    let (tx, rx) = mpsc::channel::<String>();
    let reader = std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            let Ok(line) = line else { break };
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    let serving = loop {
        match rx.recv_timeout(REPORT_BOUND) {
            Ok(line) if line.starts_with("SERVING ") => break Some(line),
            Ok(_) => continue,
            Err(_) => break None,
        }
    };
    let Some(serving) = serving else {
        let _ = child.0.kill();
        let status = child.0.wait();
        let mut stderr = String::new();
        if let Some(mut e) = child.0.stderr.take() {
            let _ = e.read_to_string(&mut stderr);
        }
        panic!("no SERVING line ({status:?}); stderr:\n{stderr}");
    };
    let addr: SocketAddr = field(&serving, "addr").parse().expect("a socket address");
    let metrics: SocketAddr = field(&serving, "metrics")
        .parse()
        .expect("a socket address");
    assert!(addr.port() != 0 && metrics.port() != 0, "{serving}");
    assert_ne!(addr, metrics, "{serving}");

    // Both doors are open at the reported ports the moment the line is
    // out (the line follows the binds).
    drop(TcpStream::connect(addr).expect("the game door accepts"));
    let mut stream = TcpStream::connect(metrics).expect("the metric stream accepts");
    let mut bytes = Vec::new();
    stream
        .read_to_end(&mut bytes)
        .expect("the stream ends at the clean stop");
    // One framed report at least: [u32 magic][u32 body length][body]
    // (the magic moves with the wire's version, so only the frame's
    // shape is checked here).
    assert!(bytes.len() >= 8, "a framed metric report arrived");
    let body = u32::from_le_bytes(bytes[4..8].try_into().unwrap()) as usize;
    assert!(
        body > 0 && bytes.len() >= 8 + body,
        "the first frame is whole"
    );

    let status = child.0.wait().expect("the child exits");
    assert!(
        status.success(),
        "the served server stops cleanly: {status:?}"
    );
    reader.join().expect("the stdout reader");
}

/// How long a served server nobody reads may take to exit after its
/// `--duration` before the test calls it hung: the export's bound after
/// the stop (2 s, `serve.rs`) plus the stop's own bounds (3 s at most),
/// with slack for a loaded machine. Only a hang guard — the claim is that
/// the child exits at all.
const EXIT_BOUND: Duration = Duration::from_secs(20);

/// Nobody connects to the metric stream (a hand-run `--serve`, an
/// orchestrator that died): the served server still exits after its
/// duration, cleanly — its export stops waiting for a reader a bounded
/// time after the stop (BACKLOG F40). It used to wait on `accept`
/// forever.
#[test]
fn a_served_server_nobody_reads_still_exits() {
    let mut child = Reaped(
        Command::new(env!("CARGO_BIN_EXE_gsb-loadgen"))
            .args(["--serve", "--bind", "127.0.0.1:0"])
            .args(["--metrics-listen", "127.0.0.1:0", "--duration", "1"])
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawning gsb-loadgen --serve"),
    );
    let deadline = Instant::now() + Duration::from_secs(1) + EXIT_BOUND;
    let status = loop {
        if let Some(status) = child.0.try_wait().expect("polling the child") {
            break Some(status);
        }
        if Instant::now() >= deadline {
            break None;
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    let Some(status) = status else {
        let _ = child.0.kill();
        let _ = child.0.wait();
        let mut stderr = String::new();
        if let Some(mut e) = child.0.stderr.take() {
            let _ = e.read_to_string(&mut stderr);
        }
        panic!(
            "the served server did not exit with nobody on its metric stream; stderr:\n{stderr}"
        );
    };
    assert!(
        status.success(),
        "the served server stops cleanly: {status:?}"
    );
}
