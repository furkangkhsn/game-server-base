//! The one response a connection gets: status line, the minimal header
//! set (`Connection: close`) and a text body.

/// One response: status line + the minimal header set + a text body.
pub(super) struct Response {
    pub(super) status: u16,
    pub(super) reason: &'static str,
    pub(super) content_type: &'static str,
    pub(super) allow: Option<&'static str>,
    pub(super) body: String,
}

impl Response {
    pub(super) fn text(status: u16, reason: &'static str, body: impl Into<String>) -> Self {
        Self {
            status,
            reason,
            content_type: "text/plain; charset=utf-8",
            allow: None,
            body: body.into(),
        }
    }

    pub(super) fn method_not_allowed(allow: &'static str) -> Self {
        let mut r = Self::text(405, "Method Not Allowed", "method not allowed\n");
        r.allow = Some(allow);
        r
    }

    pub(super) fn serialize(&self) -> Vec<u8> {
        use std::fmt::Write as _;
        let mut head = String::with_capacity(160 + self.body.len());
        let _ = write!(
            head,
            "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n",
            self.status,
            self.reason,
            self.content_type,
            self.body.len()
        );
        if let Some(allow) = self.allow {
            let _ = write!(head, "Allow: {allow}\r\n");
        }
        head.push_str("\r\n");
        let mut out = head.into_bytes();
        out.extend_from_slice(self.body.as_bytes());
        out
    }
}
