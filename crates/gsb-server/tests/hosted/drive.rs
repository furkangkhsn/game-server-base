//! Driving many clients at once: wait for a condition, or hold one for
//! a while, draining every client in between; and config files on disk.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use super::{Client, View};

/// Drain every client every few milliseconds until `done` holds, or
/// panic naming `what` after `within`.
pub async fn eventually<V: View>(
    clients: &mut [&mut Client<V>],
    within: Duration,
    what: &str,
    mut done: impl FnMut(&[&mut Client<V>]) -> bool,
) {
    let deadline = Instant::now() + within;
    loop {
        for c in clients.iter_mut() {
            c.drain();
        }
        if done(clients) {
            return;
        }
        assert!(Instant::now() < deadline, "timed out: {what}");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// Drain every client for `window`, checking `invariant` after each pass.
pub async fn hold<V: View>(
    clients: &mut [&mut Client<V>],
    window: Duration,
    mut invariant: impl FnMut(&[&mut Client<V>]),
) {
    let deadline = Instant::now() + window;
    while Instant::now() < deadline {
        for c in clients.iter_mut() {
            c.drain();
        }
        invariant(clients);
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// Write `text` to a fresh temp file and load it as a config FILE (so
/// `Config::raw` holds exactly what was written).
pub fn config_file(tag: &str, text: &str) -> gsb_server::Config {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let n = SEQ.fetch_add(1, Ordering::SeqCst);
    let path: PathBuf =
        std::env::temp_dir().join(format!("gsb-hosted-{tag}-{}-{n}.toml", std::process::id()));
    std::fs::write(&path, text).expect("write config");
    let cfg = gsb_server::Config::from_file(&path).expect("config parses");
    let _ = std::fs::remove_file(&path);
    cfg
}
