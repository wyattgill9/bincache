//! What each route does, and the reasons behind the headers it sets.
//!
//! Two rules are load-bearing on the payload plane and are asserted by conformance tests
//! rather than left to review:
//!
//! - `Accept-Ranges: bytes` is mandatory. `maybeRetry` in `filetransfer.cc` resumes a
//!   dropped NAR only if the first response advertised it.
//! - `Content-Encoding` is never set. curl asks for every encoding it supports, and
//!   answering would double-encode against the narinfo `Compression` contract *and*
//!   silently disable resume. A dropped connection 9 GB into a 10 GB NAR would then
//!   restart at zero, which presents as a throughput cliff and never as an error.
//!
//! Routing goes through [`crate::route::resolve`] under a single `fallback` rather than an
//! axum route table. `resolve` stays the one owner of URL shape, and dispatching on the
//! method here avoids axum's `MethodRouter`, which answers `HEAD` by running the `GET`
//! handler and discarding the body. `HEAD /nar/<nar hash>.nar` must not do that: it is the
//! probe a client runs before uploading, it is answered from the NAR index rather than from
//! the filesystem, and getting it wrong makes every build node re-upload every NAR forever.

use snafu::ResultExt as _;

/// Content type for `nix-cache-info`, which nix reads as plain key-value text.
const CACHE_INFO_TYPE: &str = "text/x-nix-cache-info";

/// Prometheus text exposition version, for `GET /metrics`.
const METRICS_TYPE: &str = "text/plain; version=0.0.4";

/// Content type for a refusal body, which is one line of prose for a human.
const REFUSAL_TYPE: &str = "text/plain; charset=utf-8";

/// Announced on every response, so an operator can tell what answered.
const SERVER: &str = "bincache";

/// Ceiling on a narinfo `PUT` body. A real narinfo is around 1 KB; a megabyte is four
/// orders of magnitude of headroom and still bounds the buffer.
pub const METADATA_BODY_MAX: usize = 1024 * 1024;

#[derive(Debug, snafu::Snafu)]
#[snafu(visibility(pub))]
pub enum Error {
    #[snafu(display("a published narinfo could not be read"))]
    Narinfo { source: bincache_store::narinfo::Error },

    #[snafu(display("a NAR receipt could not be read"))]
    Receipt { source: bincache_store::receipt::Error },

    #[snafu(display("the payload store could not be read"))]
    Store { source: bincache_store::nar::Error },

    #[snafu(display("the upload failed"))]
    Upload { source: bincache_ingest::upload::Error },

    #[snafu(display("the response could not be built"))]
    Build { source: axum::http::Error },
}

/// A server fault. The pusher or reader cannot act on any of these, so the reason stays in
/// the log and the wire gets a bare `500`.
impl axum::response::IntoResponse for Error {
    fn into_response(self) -> axum::response::Response {
        tracing::error!(error = ?self, "the request could not be answered");
        bare(axum::http::StatusCode::INTERNAL_SERVER_ERROR)
    }
}

/// What the composition root assembles a [`Cache`] from.
pub struct Parts {
    pub ingest: bincache_ingest::ingest::Ingest,
    pub tokens: bincache_ingest::auth::Tokens,
    pub info: bincache_core::cacheinfo::CacheInfo,
    pub stats: crate::stats::Stats,
}

/// Everything a request needs to be answered. Cheap to clone; axum hands one to every task.
#[derive(Clone)]
pub struct Cache {
    ingest: bincache_ingest::ingest::Ingest,
    tokens: bincache_ingest::auth::Tokens,
    /// Rendered once at boot: it is a pure function of configuration.
    info: bytes::Bytes,
    stats: crate::stats::Stats,
}

/// A refused push, resolved into the two things the wire needs.
///
/// Building this is the one place a typed ingest error becomes a string, which is why the
/// conversion lives here and not in the crate that raised it.
struct Refusal {
    status: axum::http::StatusCode,
    /// `None` for a server fault: the pusher cannot act on it, and the text names paths
    /// inside the data directory.
    message: Option<String>,
}

impl Refusal {
    fn new(fault: bincache_ingest::fault::Fault, error: &dyn core::fmt::Display) -> Self {
        match fault {
            bincache_ingest::fault::Fault::Client => Self {
                status: axum::http::StatusCode::BAD_REQUEST,
                // Trailing newline so the text lands cleanly in a terminal when a client
                // does print it.
                message: Some(format!("{error}\n")),
            },
            bincache_ingest::fault::Fault::Server => {
                Self { status: axum::http::StatusCode::INTERNAL_SERVER_ERROR, message: None }
            }
        }
    }

    fn into_response(self) -> axum::response::Response {
        let Self { status, message } = self;
        let Some(message) = message else {
            return bare(status);
        };
        match body(status, REFUSAL_TYPE, bytes::Bytes::from(message)) {
            Ok(response) => response,
            Err(error) => axum::response::IntoResponse::into_response(error),
        }
    }
}

impl Cache {
    #[must_use]
    pub fn new(parts: Parts) -> Self {
        let Parts { ingest, tokens, info, stats } = parts;
        Self { ingest, tokens, info: bytes::Bytes::from(info.render()), stats }
    }

    #[must_use]
    pub const fn stats(&self) -> &crate::stats::Stats {
        &self.stats
    }

    #[must_use]
    pub const fn ingest(&self) -> &bincache_ingest::ingest::Ingest {
        &self.ingest
    }
}

/// The whole protocol surface, behind one dispatcher. See the module doc for why this is a
/// `fallback` rather than a route table.
pub fn router(cache: Cache) -> axum::Router {
    axum::Router::new()
        // A NAR is multi-gigabyte and is bounded by the declared `Content-Length` that
        // hyper enforces, plus the hash check that gates anything durable. The narinfo
        // route applies its own, much smaller, ceiling.
        .fallback(dispatch)
        .layer(axum::extract::DefaultBodyLimit::disable())
        .with_state(cache)
}

async fn dispatch(
    axum::extract::State(cache): axum::extract::State<Cache>,
    request: axum::extract::Request,
) -> axum::response::Response {
    cache.stats.request();
    let (parts, body) = request.into_parts();

    let route = match crate::route::resolve(parts.uri.path()) {
        Ok(route) => route,
        Err(crate::route::Error::Unknown { .. }) => {
            return bare(axum::http::StatusCode::NOT_FOUND);
        }
        Err(error) => {
            tracing::debug!(target = %parts.uri, error = ?error, "unroutable request");
            return bare(axum::http::StatusCode::BAD_REQUEST);
        }
    };

    let answered = match route {
        crate::route::Route::CacheInfo => cache_info(&cache),
        crate::route::Route::Metrics => metrics(&cache),
        crate::route::Route::Narinfo(key) => narinfo(&cache, &parts, body, key).await,
        crate::route::Route::Nar(url) => nar(&cache, &parts, body, &url).await,
    };
    match answered {
        Ok(response) => response,
        Err(error) => axum::response::IntoResponse::into_response(error),
    }
}

fn cache_info(cache: &Cache) -> Result<axum::response::Response, Error> {
    body(axum::http::StatusCode::OK, CACHE_INFO_TYPE, cache.info.clone())
}

fn metrics(cache: &Cache) -> Result<axum::response::Response, Error> {
    let rendered = cache.stats.total().render(cache.ingest.paths());
    body(axum::http::StatusCode::OK, METRICS_TYPE, bytes::Bytes::from(rendered))
}

/// `GET` and `HEAD` render the same body; hyper suppresses the bytes for `HEAD` and keeps
/// the length, which is the whole reason the stored artifact is a body rather than a framed
/// response.
async fn narinfo(
    cache: &Cache,
    parts: &axum::http::request::Parts,
    request: axum::body::Body,
    key: bincache_core::storepath::Hash,
) -> Result<axum::response::Response, Error> {
    if parts.method == axum::http::Method::PUT {
        return publish(cache, parts, request).await;
    }

    let published = cache.ingest.narinfo().read(&key).await.context(NarinfoSnafu)?;
    let Some(published) = published else {
        cache.stats.metadata_miss();
        return Ok(bare(axum::http::StatusCode::NOT_FOUND));
    };
    cache.stats.metadata_hit();

    // The stored bytes are the served bytes. Nothing is decoded, re-rendered, or re-signed
    // per request; the record was rendered once, at publish, from fields ingest verified.
    body(
        axum::http::StatusCode::OK,
        bincache_core::narinfo::CONTENT_TYPE,
        bytes::Bytes::from(published),
    )
}

async fn nar(
    cache: &Cache,
    parts: &axum::http::request::Parts,
    request: axum::body::Body,
    url: &bincache_core::narurl::NarUrl,
) -> Result<axum::response::Response, Error> {
    match parts.method {
        axum::http::Method::PUT => receive(cache, parts, request, url).await,
        axum::http::Method::HEAD => probe(cache, url).await,
        _ => stream(cache, parts, url).await,
    }
}

/// The existence check `BinaryCacheStore::addToStore` runs before it uploads.
///
/// A client that compressed nothing asks about `nar/<nar hash>.nar`, but bincache
/// recompresses on receipt and stores the result under a different name, so this is
/// answered from the NAR-hash index rather than from the filesystem. Answering `404`
/// here would make every build node re-upload every NAR forever.
async fn probe(
    cache: &Cache,
    url: &bincache_core::narurl::NarUrl,
) -> Result<axum::response::Response, Error> {
    let length = if url.compression == bincache_core::compression::Compression::None {
        cache
            .ingest
            .receipt()
            .read(&url.file_hash)
            .await
            .context(ReceiptSnafu)?
            .map(|receipt| receipt.nar_size.get())
    } else {
        let reader = cache.ingest.store().read(url).await.context(StoreSnafu)?;
        reader.map(|reader| reader.size())
    };

    let Some(length) = length else {
        return Ok(bare(axum::http::StatusCode::NOT_FOUND));
    };

    // Length without a body: this route is only reached for `HEAD`, and the point of the
    // probe is to answer without opening the artifact at all.
    head(axum::http::StatusCode::OK)
        .header(axum::http::header::CONTENT_TYPE, bincache_core::narurl::CONTENT_TYPE)
        .header(axum::http::header::ACCEPT_RANGES, "bytes")
        .header(axum::http::header::CONTENT_LENGTH, length)
        .body(axum::body::Body::empty())
        .context(BuildSnafu)
}

async fn stream(
    cache: &Cache,
    parts: &axum::http::request::Parts,
    url: &bincache_core::narurl::NarUrl,
) -> Result<axum::response::Response, Error> {
    let Some(reader) = cache.ingest.store().read(url).await.context(StoreSnafu)? else {
        return Ok(bare(axum::http::StatusCode::NOT_FOUND));
    };
    let size = reader.size();
    if size == 0 {
        // Ingest refuses an empty NAR, so a zero-length artifact means a truncated file
        // on disk rather than something a client did.
        tracing::error!(name = url.name(), "artifact on disk is empty");
        return Ok(bare(axum::http::StatusCode::INTERNAL_SERVER_ERROR));
    }

    let requested = crate::range::resolve(range_header(parts), size);
    let span = match requested {
        crate::range::Requested::Whole => crate::range::Span { first: 0, last: size - 1 },
        crate::range::Requested::Partial(span) => span,
        crate::range::Requested::Unsatisfiable => {
            return head(axum::http::StatusCode::RANGE_NOT_SATISFIABLE)
                .header(axum::http::header::CONTENT_RANGE, format!("bytes */{size}"))
                .header(axum::http::header::ACCEPT_RANGES, "bytes")
                .header(axum::http::header::CONTENT_LENGTH, 0)
                .body(axum::body::Body::empty())
                .context(BuildSnafu);
        }
    };

    let partial = span.len() != size;
    let status =
        if partial { axum::http::StatusCode::PARTIAL_CONTENT } else { axum::http::StatusCode::OK };

    let mut response = head(status)
        .header(axum::http::header::CONTENT_TYPE, bincache_core::narurl::CONTENT_TYPE)
        .header(axum::http::header::ACCEPT_RANGES, "bytes")
        .header(axum::http::header::CONTENT_LENGTH, span.len());
    if partial {
        response = response.header(
            axum::http::header::CONTENT_RANGE,
            format!("bytes {}-{}/{size}", span.first, span.last),
        );
    }

    let file = reader.seek(span.first).await.context(StoreSnafu)?;
    let bounded = tokio::io::AsyncReadExt::take(file, span.len());
    let payload = axum::body::Body::from_stream(tokio_util::io::ReaderStream::new(bounded));

    cache.stats.payload(span.len());
    response.body(payload).context(BuildSnafu)
}

/// `PUT nar/<nar hash>.nar`, the payload half of a push.
async fn receive(
    cache: &Cache,
    parts: &axum::http::request::Parts,
    request: axum::body::Body,
    url: &bincache_core::narurl::NarUrl,
) -> Result<axum::response::Response, Error> {
    if admit(cache, parts) == bincache_ingest::auth::Admission::Denied {
        return Ok(bare(axum::http::StatusCode::UNAUTHORIZED));
    }

    let mut upload = match cache.ingest.receive(url).await {
        Ok(upload) => upload,
        Err(error) => {
            tracing::warn!(error = ?error, "refused an upload");
            return Ok(Refusal::new(error.fault(), &error).into_response());
        }
    };

    let mut chunks = request.into_data_stream();
    loop {
        let next = futures_util::StreamExt::next(&mut chunks).await;
        let Some(chunk) = next else {
            break;
        };
        let Ok(chunk) = chunk else {
            // The peer vanished or framed its body badly. Dropping `upload` unlinks the
            // staging file, so nothing durable was ever named after these bytes.
            tracing::debug!("an upload ended before its body did");
            return Ok(bare(axum::http::StatusCode::BAD_REQUEST));
        };
        upload.write(&chunk).await.context(UploadSnafu)?;
    }

    let compressed = upload.finish().await.context(UploadSnafu)?;
    let verified = match compressed.verify() {
        Ok(verified) => verified,
        Err(error) => {
            // The staging file goes away with the dropped upload; nothing durable
            // was ever named after unverified content.
            tracing::warn!(error = ?error, "upload failed verification");
            return Ok(Refusal::new(error.fault(), &error).into_response());
        }
    };
    verified.store(cache.ingest.store(), cache.ingest.receipt()).await.context(UploadSnafu)?;

    cache.stats.upload();
    Ok(bare(axum::http::StatusCode::CREATED))
}

/// `PUT <hash>.narinfo`, the metadata half of a push, and the publish.
async fn publish(
    cache: &Cache,
    parts: &axum::http::request::Parts,
    request: axum::body::Body,
) -> Result<axum::response::Response, Error> {
    if admit(cache, parts) == bincache_ingest::auth::Admission::Denied {
        return Ok(bare(axum::http::StatusCode::UNAUTHORIZED));
    }
    if declared_length(parts).is_some_and(|length| length > METADATA_BODY_MAX) {
        return Ok(bare(axum::http::StatusCode::PAYLOAD_TOO_LARGE));
    }

    let Ok(collected) = axum::body::to_bytes(request, METADATA_BODY_MAX).await else {
        return Ok(bare(axum::http::StatusCode::BAD_REQUEST));
    };
    let Ok(text) = String::from_utf8(collected.into()) else {
        return Ok(bare(axum::http::StatusCode::BAD_REQUEST));
    };

    if let Err(error) = cache.ingest.publish(&text).await {
        tracing::warn!(error = ?error, "refused a narinfo");
        return Ok(Refusal::new(error.fault(), &error).into_response());
    }
    Ok(bare(axum::http::StatusCode::CREATED))
}

fn admit(cache: &Cache, parts: &axum::http::request::Parts) -> bincache_ingest::auth::Admission {
    let admission = parts
        .headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(bincache_ingest::auth::credential)
        .map_or(bincache_ingest::auth::Admission::Denied, |token| cache.tokens.admits(&token));
    if admission == bincache_ingest::auth::Admission::Denied {
        cache.stats.rejection();
    }
    admission
}

fn range_header(parts: &axum::http::request::Parts) -> Option<&str> {
    parts.headers.get(axum::http::header::RANGE).and_then(|value| value.to_str().ok())
}

fn declared_length(parts: &axum::http::request::Parts) -> Option<usize> {
    parts
        .headers
        .get(axum::http::header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse().ok())
}

/// A response head under construction, carrying the fields every answer sets.
fn head(status: axum::http::StatusCode) -> axum::http::response::Builder {
    axum::http::Response::builder().status(status).header(axum::http::header::SERVER, SERVER)
}

/// A response with a body. hyper suppresses the bytes for a `HEAD` request and keeps the
/// length it computed, so callers do not special-case the method.
fn body(
    status: axum::http::StatusCode,
    content_type: &str,
    payload: bytes::Bytes,
) -> Result<axum::response::Response, Error> {
    head(status)
        .header(axum::http::header::CONTENT_TYPE, content_type)
        .header(axum::http::header::CONTENT_LENGTH, payload.len())
        .body(axum::body::Body::from(payload))
        .context(BuildSnafu)
}

/// A bodyless answer: an error status, or a miss.
fn bare(status: axum::http::StatusCode) -> axum::response::Response {
    let built =
        head(status).header(axum::http::header::CONTENT_LENGTH, 0).body(axum::body::Body::empty());
    match built {
        Ok(response) => response,
        // Unreachable: every header above is a constant. Answering rather than panicking
        // keeps a serving path free of `unwrap`.
        Err(error) => {
            tracing::error!(error = ?error, "a constant response head failed to build");
            axum::http::Response::new(axum::body::Body::empty())
        }
    }
}
