//! The served server (`gsb-loadgen --serve`) reports the addresses it
//! actually bound (BACKLOG F31). The orchestrator used to pick its
//! child's ports itself — bind port 0, read the number, close, pass it on
//! — and a port taken by anyone else between that close and the child's
//! bind killed the child with "Address already in use". The child now
//! binds port 0 and says which ports it got; this is that report, seen
//! from outside the process.

use std::io::{BufRead, BufReader, Read};
use std::net::{SocketAddr, TcpStream};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

/// How long the child may take to report — a start-up is milliseconds;
/// the bound only turns a silent child into a failure instead of a hang.
const REPORT_BOUND: Duration = Duration::from_secs(60);

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
    let mut child = Command::new(env!("CARGO_BIN_EXE_gsb-loadgen"))
        .args(["--serve", "--bind", "127.0.0.1:0"])
        .args(["--metrics-listen", "127.0.0.1:0", "--duration", "2"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawning gsb-loadgen --serve");
    let stdout = child.stdout.take().expect("stdout piped");
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
        let _ = child.kill();
        let out = child.wait_with_output().expect("the child's output");
        panic!(
            "no SERVING line (status {:?}); stderr:\n{}",
            out.status.code(),
            String::from_utf8_lossy(&out.stderr)
        );
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

    let status = child.wait().expect("the child exits");
    assert!(
        status.success(),
        "the served server stops cleanly: {status:?}"
    );
    reader.join().expect("the stdout reader");
}
