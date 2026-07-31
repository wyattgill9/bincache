//! Building a response head.
//!
//! Framing is built per request rather than baked into stored bytes, which is the decision
//! `research/DESIGN_V2.md` records under "What gets prerendered". Nothing durable knows
//! this file exists, so HTTP/2 lands here and nowhere else.

use swrite::SWrite as _;

/// Announced on every response, so an operator can tell what answered.
pub const SERVER: &str = "bincache";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    Continue,
    Ok,
    Created,
    NoContent,
    PartialContent,
    BadRequest,
    Unauthorized,
    NotFound,
    MethodNotAllowed,
    LengthRequired,
    ContentTooLarge,
    RangeNotSatisfiable,
    ServerError,
    NotImplemented,
}

impl Status {
    /// Code and reason phrase together, since they are one fact about a status and would
    /// drift apart if they were two tables.
    const fn line(self) -> (u16, &'static str) {
        match self {
            Self::Continue => (100, "Continue"),
            Self::Ok => (200, "OK"),
            Self::Created => (201, "Created"),
            Self::NoContent => (204, "No Content"),
            Self::PartialContent => (206, "Partial Content"),
            Self::BadRequest => (400, "Bad Request"),
            Self::Unauthorized => (401, "Unauthorized"),
            Self::NotFound => (404, "Not Found"),
            Self::MethodNotAllowed => (405, "Method Not Allowed"),
            Self::LengthRequired => (411, "Length Required"),
            Self::ContentTooLarge => (413, "Content Too Large"),
            Self::RangeNotSatisfiable => (416, "Range Not Satisfiable"),
            Self::ServerError => (500, "Internal Server Error"),
            Self::NotImplemented => (501, "Not Implemented"),
        }
    }

    #[must_use]
    pub const fn code(self) -> u16 {
        self.line().0
    }

    #[must_use]
    pub const fn reason(self) -> &'static str {
        self.line().1
    }
}

/// A response head under construction. Headers are appended in order; the terminating
/// blank line is written by [`Head::finish`], so a half-built head cannot be sent.
#[must_use]
pub struct Head {
    text: String,
}

impl Head {
    pub fn new(status: Status, keep_alive: crate::http::KeepAlive) -> Self {
        let mut text = String::with_capacity(256);
        swrite::swriteln!(text, "HTTP/1.1 {} {}\r", status.code(), status.reason());
        swrite::swriteln!(text, "Server: {SERVER}\r");
        let connection = match keep_alive {
            crate::http::KeepAlive::Keep => "keep-alive",
            crate::http::KeepAlive::Close => "close",
        };
        swrite::swriteln!(text, "Connection: {connection}\r");
        Self { text }
    }

    pub fn header(&mut self, name: &str, value: impl core::fmt::Display) -> &mut Self {
        swrite::swriteln!(self.text, "{name}: {value}\r");
        self
    }

    pub fn length(&mut self, bytes: u64) -> &mut Self {
        self.header("Content-Length", bytes)
    }

    /// Closes the head. The body, if any, follows the returned bytes.
    #[must_use]
    pub fn finish(mut self) -> Vec<u8> {
        self.text.push_str("\r\n");
        self.text.into_bytes()
    }

    /// Head and body in one buffer, so a small response is one write rather than two.
    #[must_use]
    pub fn with_body(mut self, body: &[u8]) -> Vec<u8> {
        self.length(u64::try_from(body.len()).unwrap_or(u64::MAX));
        let mut bytes = self.finish();
        bytes.extend_from_slice(body);
        bytes
    }
}

/// A bodyless answer: an error status, or a `HEAD` whose length is already known.
#[must_use]
pub fn bare(status: Status, keep_alive: crate::http::KeepAlive, length: u64) -> Vec<u8> {
    let mut head = Head::new(status, keep_alive);
    head.length(length);
    head.finish()
}

#[cfg(test)]
mod tests {
    #[test]
    fn renders_a_narinfo_head() {
        let mut head = crate::http::response::Head::new(
            crate::http::response::Status::Ok,
            crate::http::KeepAlive::Keep,
        );
        head.header("Content-Type", bincache_core::narinfo::CONTENT_TYPE);
        let bytes = head.with_body(b"StorePath: /nix/store/x\n");

        expect_test::expect![[r#"
            HTTP/1.1 200 OK
            Server: bincache
            Connection: keep-alive
            Content-Type: text/x-nix-narinfo
            Content-Length: 24

            StorePath: /nix/store/x
        "#]]
        .assert_eq(&String::from_utf8(bytes).expect("utf8").replace('\r', ""));
    }

    #[test]
    fn a_bare_response_carries_a_length_and_no_body() {
        let bytes = crate::http::response::bare(
            crate::http::response::Status::NotFound,
            crate::http::KeepAlive::Close,
            0,
        );
        expect_test::expect![[r#"
            HTTP/1.1 404 Not Found
            Server: bincache
            Connection: close
            Content-Length: 0

        "#]]
        .assert_eq(&String::from_utf8(bytes).expect("utf8").replace('\r', ""));
    }
}
