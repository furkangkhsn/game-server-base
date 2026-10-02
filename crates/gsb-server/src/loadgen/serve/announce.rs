//! The served server's `SERVING` line (BACKLOG F31): the addresses it
//! actually bound, on stdout, once every door it opens is bound.
//!
//! ```text
//! SERVING addr=127.0.0.1:41873 metrics=127.0.0.1:41874 udp_key=9f0c…(64 hex)
//! ```
//!
//! `metrics=-` when the child streams no metric reports (no
//! `--metrics-listen`); `udp_key=` is the sealed rUDP door's public key
//! (B5a; what the orchestrator's client children pin), `-` without one
//! (a missing field reads as `-`). The orchestrator starts its server child on port
//! 0 for both and learns the real ports from this line; before it, it
//! picked free ports itself (bind 0, read, close) and a port taken by
//! anyone else before the child's bind killed the child. An operator
//! running `--serve --bind 127.0.0.1:0` by hand reads the same line.

use std::net::SocketAddr;

/// The line's first word (and the space after it).
pub(crate) const SERVING_PREFIX: &str = "SERVING ";

/// The addresses a served server reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Serving {
    /// The game door (the first listener's bound address).
    pub(crate) addr: SocketAddr,
    /// The metric stream's listener, when the child serves one.
    pub(crate) metrics: Option<SocketAddr>,
    /// The sealed rUDP door's public key (B5a), when it has one.
    pub(crate) udp_key: Option<[u8; 32]>,
}

impl Serving {
    /// The line, without its newline.
    pub(crate) fn line(&self) -> String {
        let metrics = self
            .metrics
            .map_or_else(|| "-".to_string(), |m| m.to_string());
        let udp_key = self
            .udp_key
            .map_or_else(|| "-".to_string(), |k| gsb_server::udp_key_hex(&k));
        format!(
            "{SERVING_PREFIX}addr={} metrics={metrics} udp_key={udp_key}",
            self.addr
        )
    }

    /// Parse a line [`Serving::line`] wrote; `None` for any other line
    /// (a log line the child printed first) or a malformed one.
    pub(crate) fn parse(line: &str) -> Option<Self> {
        let rest = line.trim_end().strip_prefix(SERVING_PREFIX)?;
        let get = |k: &str| {
            rest.split_whitespace()
                .find_map(|p| p.strip_prefix(k)?.strip_prefix('='))
        };
        let addr = get("addr")?.parse().ok()?;
        let metrics = match get("metrics")? {
            "-" => None,
            m => Some(m.parse().ok()?),
        };
        let udp_key = match get("udp_key").unwrap_or("-") {
            "-" => None,
            k => Some(gsb_server::parse_udp_public_key(k).ok()?),
        };
        Some(Self {
            addr,
            metrics,
            udp_key,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The line round-trips, with and without a metric stream.
    #[test]
    fn the_line_round_trips() {
        let addr = "127.0.0.1:41873".parse().unwrap();
        for metrics in [None, Some("127.0.0.1:41874".parse().unwrap())] {
            for udp_key in [None, Some([0xAB; 32])] {
                let s = Serving {
                    addr,
                    metrics,
                    udp_key,
                };
                assert_eq!(Serving::parse(&s.line()), Some(s));
                assert_eq!(Serving::parse(&format!("{}\n", s.line())), Some(s));
            }
        }
        let plain = Serving {
            addr,
            metrics: None,
            udp_key: None,
        };
        assert_eq!(
            plain.line(),
            "SERVING addr=127.0.0.1:41873 metrics=- udp_key=-"
        );
        assert_eq!(
            Serving::parse("SERVING addr=127.0.0.1:41873 metrics=-"),
            Some(plain),
            "a line without the key (an older child) reads as no key"
        );
    }

    /// Anything else is not the line: another line, a missing or bad
    /// field.
    #[test]
    fn other_lines_are_not_the_line() {
        for line in [
            "",
            "2026-09-28T00:00:00Z  WARN gsb_server: something",
            "SERVING",
            "SERVING addr=127.0.0.1:1",
            "SERVING metrics=-",
            "SERVING addr=nowhere metrics=-",
            "SERVING addr=127.0.0.1:1 metrics=nowhere",
            "SERVING addr=127.0.0.1:1 metrics=- udp_key=nothex",
            "serving addr=127.0.0.1:1 metrics=-",
        ] {
            assert_eq!(Serving::parse(line), None, "{line:?}");
        }
    }
}
