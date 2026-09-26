//! The push target: an `http://` URL reduced to what one HTTP/1.1
//! request needs, and the request itself.

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use super::OtlpError;

/// OTLP/HTTP's metrics path, used when the URL names none.
const DEFAULT_PATH: &str = "/v1/metrics";

/// The most of a response read to find its status line.
const STATUS_LINE_MAX: usize = 1024;

/// A parsed `http://host[:port][/path]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Endpoint {
    /// The host to connect to (an IPv6 literal without its brackets).
    pub(super) host: String,
    pub(super) port: u16,
    /// The `Host` header: the URL's authority as written.
    pub(super) authority: String,
    pub(super) path: String,
}

/// Why a push failed.
#[derive(Debug)]
pub(super) enum PushError {
    Io(std::io::Error),
    /// The peer answered, but not with a 2xx.
    Status(u16),
    /// The peer's answer had no parseable status line.
    Malformed,
}

impl std::fmt::Display for PushError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PushError::Io(e) => write!(f, "{e}"),
            PushError::Status(s) => write!(f, "status {s}"),
            PushError::Malformed => f.write_str("malformed response"),
        }
    }
}

impl Endpoint {
    pub(super) fn parse(url: &str) -> Result<Self, OtlpError> {
        let bad = |why| OtlpError::BadEndpoint(url.to_owned(), why);
        if url.starts_with("https://") {
            return Err(OtlpError::Https(url.to_owned()));
        }
        let rest = url.strip_prefix("http://").ok_or(bad("not http://"))?;
        let (authority, path) = match rest.find('/') {
            Some(i) => (&rest[..i], &rest[i..]),
            None => (rest, ""),
        };
        let path = if path.is_empty() || path == "/" {
            DEFAULT_PATH
        } else {
            path
        };
        let (host, port) = if let Some(v6) = authority.strip_prefix('[') {
            let (host, tail) = v6.split_once(']').ok_or(bad("unclosed ["))?;
            match tail {
                "" => (host, None),
                _ => (host, Some(tail.strip_prefix(':').ok_or(bad("bad port"))?)),
            }
        } else {
            match authority.rsplit_once(':') {
                Some((host, port)) => (host, Some(port)),
                None => (authority, None),
            }
        };
        if host.is_empty() {
            return Err(bad("no host"));
        }
        let port = match port {
            Some(p) => p.parse().map_err(|_| bad("bad port"))?,
            None => 80,
        };
        if path.contains(char::is_whitespace) {
            return Err(bad("whitespace in the path"));
        }
        Ok(Self {
            host: host.to_owned(),
            port,
            authority: authority.to_owned(),
            path: path.to_owned(),
        })
    }

    /// POST `body` as OTLP protobuf; `Ok` on a 2xx answer.
    pub(super) async fn post(&self, body: &[u8]) -> Result<(), PushError> {
        let mut stream = TcpStream::connect((self.host.as_str(), self.port))
            .await
            .map_err(PushError::Io)?;
        let head = format!(
            "POST {} HTTP/1.1\r\nHost: {}\r\nContent-Type: application/x-protobuf\r\n\
             Content-Length: {}\r\nConnection: close\r\nUser-Agent: gsb/{}\r\n\r\n",
            self.path,
            self.authority,
            body.len(),
            env!("CARGO_PKG_VERSION"),
        );
        stream
            .write_all(head.as_bytes())
            .await
            .map_err(PushError::Io)?;
        stream.write_all(body).await.map_err(PushError::Io)?;
        // Only the status line matters (the body of a success is an
        // optional partial-success report this exporter does not act on).
        let mut buf = Vec::with_capacity(128);
        let mut chunk = [0u8; 256];
        while !buf.windows(2).any(|w| w == b"\r\n") && buf.len() < STATUS_LINE_MAX {
            let n = stream.read(&mut chunk).await.map_err(PushError::Io)?;
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&chunk[..n]);
        }
        let line = buf.split(|&b| b == b'\r').next().unwrap_or_default();
        let line = std::str::from_utf8(line).map_err(|_| PushError::Malformed)?;
        let mut parts = line.split(' ');
        let status: u16 = match (parts.next(), parts.next()) {
            (Some(v), Some(code)) if v.starts_with("HTTP/1.") => {
                code.parse().map_err(|_| PushError::Malformed)?
            }
            _ => return Err(PushError::Malformed),
        };
        if (200..300).contains(&status) {
            Ok(())
        } else {
            Err(PushError::Status(status))
        }
    }
}
