//! Running a binary that is expected to refuse startup: bounded (a
//! binary that starts after all is killed and fails the test instead of
//! hanging it), with its stderr kept for the assertions.

use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// How long a refusal may take; every case refuses before binding, in
/// milliseconds — the bound only turns a server that STARTED into a
/// failure instead of a hang.
const REFUSAL_BOUND: Duration = Duration::from_secs(20);

/// One finished run of a binary.
pub struct Run {
    /// Its exit status (`None`: killed by a signal).
    pub status: Option<i32>,
    /// Everything it wrote to stderr.
    pub stderr: String,
}

impl Run {
    /// Run `bin` with `args` until it exits (or the bound passes).
    pub fn of(bin: &str, args: &[&str]) -> Run {
        let mut child = Command::new(bin)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn the binary");
        let started = Instant::now();
        loop {
            if child.try_wait().expect("poll the child").is_some() {
                break;
            }
            if started.elapsed() > REFUSAL_BOUND {
                let _ = child.kill();
                let _ = child.wait();
                panic!("{bin} {args:?} did not refuse startup within {REFUSAL_BOUND:?}");
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let out = child.wait_with_output().expect("collect the output");
        Run {
            status: out.status.code(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        }
    }

    /// Exit status 1, and stderr carries every one of `parts` — and
    /// none of the `Debug` spellings of a returned error.
    pub fn refused_with(&self, parts: &[&str]) {
        assert_eq!(
            self.status,
            Some(1),
            "exit status; stderr:\n{}",
            self.stderr
        );
        for part in parts {
            assert!(
                self.stderr.contains(part),
                "stderr lacks {part:?}:\n{}",
                self.stderr
            );
        }
        for debug in ["Error: ", "Parse {", "input: Some("] {
            self.lacks(debug);
        }
    }

    /// Stderr does not carry `text`.
    pub fn lacks(&self, text: &str) {
        assert!(
            !self.stderr.contains(text),
            "stderr carries {text:?}:\n{}",
            self.stderr
        );
    }
}

/// Run the server binary on a config file holding `body`.
pub fn server_with_config(name: &str, body: &str) -> Run {
    let path = std::env::temp_dir().join(format!(
        "gsb-startup-errors-{}-{name}.toml",
        std::process::id()
    ));
    std::fs::write(&path, body).expect("write the config");
    let run = server_with_path(&path);
    let _ = std::fs::remove_file(&path);
    run
}

/// Run the server binary on the config path `path`.
pub fn server_with_path(path: &Path) -> Run {
    let path = path.to_str().expect("a UTF-8 temp path");
    Run::of(env!("CARGO_BIN_EXE_gsb-server"), &[path])
}
