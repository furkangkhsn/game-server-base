//! The orchestrator learns its server child's addresses from the child's
//! own report (BACKLOG F31): the report's ports are the ones used, log
//! lines before it are passed on, and a child that dies or stays silent
//! first is a clear, bounded error.

use super::*;
use std::path::Path;

const SERVING: &str = "SERVING addr=127.0.0.1:41873 metrics=127.0.0.1:41874\n";

/// Read `input` as a child's stdout, collecting what gets forwarded.
async fn read(
    input: &[u8],
    bound: Duration,
) -> (Result<(SocketAddr, SocketAddr), ReportError>, Vec<u8>) {
    let mut forwarded = Vec::new();
    let mut reader = BufReader::new(input);
    let got = await_serving(&mut reader, bound, |l| forwarded.extend_from_slice(l)).await;
    (got, forwarded)
}

/// The report's addresses are the answer; the log lines before it are
/// forwarded untouched, the report itself is not.
#[tokio::test]
async fn the_report_names_the_addresses_and_earlier_lines_pass_on() {
    let input = format!("a log line\nanother\n{SERVING}after\n");
    let (got, forwarded) = read(input.as_bytes(), Duration::from_secs(5)).await;
    let (addr, metrics) = got.expect("the report");
    assert_eq!(addr, "127.0.0.1:41873".parse().unwrap());
    assert_eq!(metrics, "127.0.0.1:41874".parse().unwrap());
    assert_eq!(forwarded, b"a log line\nanother\n");
}

/// Stdout ending before the report is the child's exit.
#[tokio::test]
async fn an_ended_stdout_is_an_exited_child() {
    let (got, forwarded) = read(b"panicked at start\n", Duration::from_secs(5)).await;
    assert!(matches!(got, Err(ReportError::Exited)), "{got:?}");
    assert_eq!(forwarded, b"panicked at start\n");
}

/// A report that does not parse, or names no metric stream, is refused
/// at once — not waited past.
#[tokio::test]
async fn an_unusable_report_is_refused() {
    for line in [
        "SERVING addr=127.0.0.1:41873 metrics=-\n",
        "SERVING addr=127.0.0.1:0x metrics=127.0.0.1:1\n",
    ] {
        let (got, _) = read(line.as_bytes(), Duration::from_secs(5)).await;
        assert!(
            matches!(got, Err(ReportError::Malformed(_))),
            "{line}: {got:?}"
        );
    }
}

/// A child that neither reports nor closes stdout is given up at the
/// bound.
#[tokio::test]
async fn a_silent_child_is_given_up_at_the_bound() {
    let (_held_open, far) = tokio::io::duplex(64);
    let mut reader = BufReader::new(far);
    let got = await_serving(&mut reader, Duration::from_millis(100), |_| {}).await;
    assert!(matches!(got, Err(ReportError::Silent(_))), "{got:?}");
}

/// Start a stand-in "server child": a shell running `script`.
async fn start(script: &str, bound: Duration) -> Result<ServerChild, String> {
    let argv = vec!["-c".to_string(), script.to_string()];
    start_server_child(Path::new("/bin/sh"), &argv, &None, None, bound).await
}

/// End-to-end over a real child process: the addresses the orchestrator
/// aims its clients at and reads its metrics from are exactly the ones
/// the child reported — nothing picked beforehand.
#[tokio::test]
async fn a_real_child_is_used_at_the_ports_it_reported() {
    let script = format!("echo 'a log line'; printf '{SERVING}'; exec sleep 30");
    let mut s = start(&script, Duration::from_secs(30))
        .await
        .expect("started");
    assert_eq!(s.addr, "127.0.0.1:41873".parse().unwrap());
    assert_eq!(s.metrics, "127.0.0.1:41874".parse().unwrap());
    s.child.kill().await.expect("kill the stand-in");
    let _ = s.child.wait().await;
    let _ = tokio::time::timeout(Duration::from_secs(5), s.stdout_forward).await;
}

/// A child that dies before reporting (in the wild: a refused config or
/// bind) is a clear error carrying its exit status — no client child is
/// ever aimed at it.
#[tokio::test]
async fn a_child_that_dies_first_is_a_clear_error() {
    let err = start("echo 'refused' >&2; exit 3", Duration::from_secs(30))
        .await
        .err()
        .expect("no server");
    assert!(
        err.contains("exited before reporting its addresses"),
        "{err}"
    );
    assert!(err.contains('3'), "the exit status is named: {err}");
}

/// A child that stays silent is killed at the bound and reaped.
#[tokio::test]
async fn a_silent_child_is_killed_at_the_bound() {
    let err = start("exec sleep 30", Duration::from_millis(300))
        .await
        .err()
        .expect("no server");
    assert!(err.contains("reported no addresses within"), "{err}");
    assert!(err.contains("signal"), "killed, then reaped: {err}");
}
