//! Hand-rolled HTTP/1.1, covering exactly the surface a Nix client uses.
//!
//! Deliberately absent, because the client evidence in `research/DESIGN_V2.md` shows they
//! are unreachable: `ETag` and `304` (the binary cache store never sends
//! `If-None-Match`), and `Content-Encoding` on any response (curl asks for every encoding
//! it supports, and answering would both double-encode against the narinfo `Compression`
//! contract and silently disable NAR resume).

pub mod response;

use snafu::OptionExt as _;
use snafu::ResultExt as _;

/// Ceiling on a request head. A Nix request head is a few hundred bytes; anything past
/// this is not a client this cache serves.
pub const HEAD_LEN_MAX: usize = 16 * 1024;

/// Ceiling on a narinfo `PUT` body. A real narinfo is around 1 KB; a megabyte is four
/// orders of magnitude of headroom and still bounds the buffer.
pub const METADATA_BODY_MAX: u64 = 1024 * 1024;

/// The blank line that ends a request head.
const HEAD_END: &[u8] = b"\r\n\r\n";

#[derive(Clone, Copy, Debug, PartialEq, Eq, strum::Display, strum::EnumString)]
#[strum(serialize_all = "UPPERCASE")]
pub enum Method {
    Get,
    Head,
    Put,
}

/// Whether the connection survives this exchange. HTTP/1.1 defaults to persistent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeepAlive {
    Keep,
    Close,
}

/// Whether the client is waiting for `100 Continue` before it sends a body. curl sets this
/// on uploads past about a kilobyte, and stalls for a second if nothing answers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Expectation {
    Continue,
    None,
}

/// How the request body is delimited. `Transfer-Encoding` is refused rather than
/// misparsed; nix always knows its upload length and sends `Content-Length`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Framing {
    Empty,
    Length(u64),
    Chunked,
}

#[derive(Debug, snafu::Snafu)]
#[snafu(visibility(pub))]
pub enum Error {
    #[snafu(display("the request head is not valid UTF-8"))]
    Encoding { source: core::str::Utf8Error },

    #[snafu(display("the request line is malformed"))]
    RequestLine,

    #[snafu(display("{method:?} is not a method this cache answers"))]
    Method { method: String },

    #[snafu(display("{version:?} is not HTTP/1.x"))]
    Version { version: String },

    #[snafu(display("header line {line:?} is malformed"))]
    Header { line: String },

    #[snafu(display("Content-Length is not an integer"))]
    Length { source: core::num::ParseIntError },

    #[snafu(display(
        "the request carries both Content-Length and Transfer-Encoding, so where its body \
         ends depends on who is reading"
    ))]
    ConflictingFraming,
}

#[derive(Debug)]
pub struct Request<'a> {
    pub method: Method,
    pub target: &'a str,
    pub framing: Framing,
    pub keep_alive: KeepAlive,
    pub expectation: Expectation,
    pub range: Option<&'a str>,
    pub authorization: Option<&'a str>,
}

/// Bytes consumed by the head, once it is complete. `None` means read more.
#[must_use]
pub fn head_len(buffer: &[u8]) -> Option<usize> {
    buffer
        .windows(HEAD_END.len())
        .position(|window| window == HEAD_END)
        .map(|at| at + HEAD_END.len())
}

/// Parses a complete request head. `head` must be exactly what [`head_len`] measured.
pub fn parse(head: &[u8]) -> Result<Request<'_>, Error> {
    let text = core::str::from_utf8(head).context(EncodingSnafu)?;
    let mut lines = text.split_terminator("\r\n");

    let request_line = lines.next().context(RequestLineSnafu)?;
    let mut parts = request_line.split(' ');
    let method = parts.next().context(RequestLineSnafu)?;
    let target = parts.next().context(RequestLineSnafu)?;
    let version = parts.next().context(RequestLineSnafu)?;
    snafu::ensure!(parts.next().is_none(), RequestLineSnafu);
    snafu::ensure!(version.starts_with("HTTP/1."), VersionSnafu { version });

    let method: Method =
        core::str::FromStr::from_str(method).ok().context(MethodSnafu { method })?;

    let mut request = Request {
        method,
        target,
        framing: Framing::Empty,
        // HTTP/1.1 is persistent unless the client says otherwise.
        keep_alive: KeepAlive::Keep,
        expectation: Expectation::None,
        range: None,
        authorization: None,
    };
    for line in lines.filter(|line| !line.is_empty()) {
        absorb(&mut request, line)?;
    }
    Ok(request)
}

fn absorb<'a>(request: &mut Request<'a>, line: &'a str) -> Result<(), Error> {
    let (name, value) = line.split_once(':').context(HeaderSnafu { line })?;
    let value = value.trim();

    // Field names are case-insensitive, and curl does not promise a spelling.
    if name.eq_ignore_ascii_case("content-length") {
        // RFC 9112 6.1: when both framings are present the message is ambiguous, and two
        // recipients that resolve it differently disagree about where this request ends and
        // the next one begins. Refusing is mandatory, and refusing in *both* orders matters:
        // assigning unconditionally here is what let the later header silently win.
        snafu::ensure!(request.framing != Framing::Chunked, ConflictingFramingSnafu);
        request.framing = Framing::Length(value.parse().context(LengthSnafu)?);
    } else if name.eq_ignore_ascii_case("transfer-encoding") {
        snafu::ensure!(!matches!(request.framing, Framing::Length(_)), ConflictingFramingSnafu);
        request.framing = Framing::Chunked;
    } else if name.eq_ignore_ascii_case("connection") {
        if value.eq_ignore_ascii_case("close") {
            request.keep_alive = KeepAlive::Close;
        } else {
            request.keep_alive = KeepAlive::Keep;
        }
    } else if name.eq_ignore_ascii_case("expect") {
        if value.eq_ignore_ascii_case("100-continue") {
            request.expectation = Expectation::Continue;
        } else {
            request.expectation = Expectation::None;
        }
    } else if name.eq_ignore_ascii_case("range") {
        request.range = Some(value);
    } else if name.eq_ignore_ascii_case("authorization") {
        request.authorization = Some(value);
    }
    Ok(())
}

impl Request<'_> {
    /// How many body bytes follow the head, for the routes that take one.
    ///
    /// Zero for a chunked body, whose length is not declared anywhere. Use
    /// [`Request::carries_body`] to ask whether a body is *there*; the two questions are
    /// different and conflating them is how a chunked body got left in the socket.
    #[must_use]
    pub const fn body_len(&self) -> u64 {
        match self.framing {
            Framing::Length(length) => length,
            Framing::Empty | Framing::Chunked => 0,
        }
    }

    /// Whether a body follows that a bodyless answer would leave unread.
    ///
    /// A chunked body has no declared length but is still on the wire, so a connection kept
    /// alive after refusing one hands the next request the chunk data. That is a request
    /// smuggling primitive, not a cosmetic bug.
    #[must_use]
    pub const fn carries_body(&self) -> bool {
        match self.framing {
            Framing::Empty => false,
            Framing::Length(length) => length > 0,
            Framing::Chunked => true,
        }
    }
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    const GET: &[u8] = b"GET /nix-cache-info HTTP/1.1\r\nHost: cache\r\n\r\n";

    #[test]
    fn measures_a_complete_head() {
        assert_eq!(crate::http::head_len(GET), Some(GET.len()));
        assert_eq!(crate::http::head_len(&GET[..GET.len() - 1]), None);
        assert_eq!(crate::http::head_len(b"GET / HTTP/1.1\r\n"), None);
    }

    #[test]
    fn parses_a_plain_get() {
        let request = crate::http::parse(GET).expect("parses");
        assert_eq!(request.method, crate::http::Method::Get);
        assert_eq!(request.target, "/nix-cache-info");
        assert_eq!(request.framing, crate::http::Framing::Empty);
        assert_eq!(request.keep_alive, crate::http::KeepAlive::Keep);
        assert_eq!(request.expectation, crate::http::Expectation::None);
    }

    #[test]
    fn reads_the_headers_a_nix_upload_sends() {
        let head = b"PUT /nar/abc.nar HTTP/1.1\r\n\
                     Content-Length: 4096\r\n\
                     Expect: 100-continue\r\n\
                     Authorization: Bearer secret\r\n\
                     Connection: close\r\n\r\n";
        let request = crate::http::parse(head).expect("parses");
        assert_eq!(request.method, crate::http::Method::Put);
        assert_eq!(request.framing, crate::http::Framing::Length(4096));
        assert_eq!(request.body_len(), 4096);
        assert_eq!(request.expectation, crate::http::Expectation::Continue);
        assert_eq!(request.authorization, Some("Bearer secret"));
        assert_eq!(request.keep_alive, crate::http::KeepAlive::Close);
    }

    #[test]
    fn header_names_are_case_insensitive() {
        let head = b"GET /x HTTP/1.1\r\nCONTENT-LENGTH: 7\r\nrAnGe: bytes=1-\r\n\r\n";
        let request = crate::http::parse(head).expect("parses");
        assert_eq!(request.framing, crate::http::Framing::Length(7));
        assert_eq!(request.range, Some("bytes=1-"));
    }

    #[test]
    fn chunked_bodies_are_recognized_rather_than_misparsed() {
        let head = b"PUT /x HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n";
        let request = crate::http::parse(head).expect("parses");
        assert_eq!(request.framing, crate::http::Framing::Chunked);
        assert_eq!(request.body_len(), 0);
    }

    #[test]
    fn refuses_methods_and_versions_it_does_not_serve() {
        let post = crate::http::parse(b"POST /x HTTP/1.1\r\n\r\n");
        assert!(matches!(post, Err(crate::http::Error::Method { .. })));

        let old = crate::http::parse(b"GET /x HTTP/0.9\r\n\r\n");
        assert!(matches!(old, Err(crate::http::Error::Version { .. })));

        let ragged = crate::http::parse(b"GET /x\r\n\r\n");
        assert!(matches!(ragged, Err(crate::http::Error::RequestLine)));
    }
}
