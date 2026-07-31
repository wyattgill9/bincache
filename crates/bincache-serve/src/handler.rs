//! What each route does, and the reasons behind the headers it sets.
//!
//! Two rules from `research/DESIGN_V2.md` are load-bearing on the payload plane and are
//! asserted by conformance tests rather than left to review:
//!
//! - `Accept-Ranges: bytes` is mandatory. `maybeRetry` in `filetransfer.cc` resumes a
//!   dropped NAR only if the first response advertised it.
//! - `Content-Encoding` is never set. curl asks for every encoding it supports, and
//!   answering would double-encode against the narinfo `Compression` contract *and*
//!   silently disable resume. A dropped connection 9 GB into a 10 GB NAR would then
//!   restart at zero, which presents as a throughput cliff and never as an error.

use snafu::ResultExt as _;

/// Content type for `nix-cache-info`, which nix reads as plain key-value text.
const CACHE_INFO_TYPE: &str = "text/x-nix-cache-info";

/// Prometheus text exposition version, for `GET /metrics`.
const METRICS_TYPE: &str = "text/plain; version=0.0.4";

#[derive(Debug, snafu::Snafu)]
#[snafu(visibility(pub))]
pub enum Error {
    #[snafu(display("the connection failed"))]
    Connection { source: crate::connection::Error },

    #[snafu(display("the index could not be read"))]
    Index { source: bincache_index::index::Error },

    #[snafu(display("the payload store could not be read"))]
    Store { source: bincache_store::nar::Error },

    #[snafu(display("the upload failed"))]
    Upload { source: bincache_ingest::upload::Error },
}

/// What the composition root assembles a [`Cache`] from.
pub struct Parts {
    pub ingest: bincache_ingest::ingest::Ingest,
    pub tokens: bincache_ingest::auth::Tokens,
    pub info: bincache_core::cacheinfo::CacheInfo,
    pub stats: crate::stats::Shards,
}

/// Everything a shard needs to answer a request. Cheap to clone; every shard holds one.
#[derive(Clone)]
pub struct Cache {
    ingest: bincache_ingest::ingest::Ingest,
    tokens: bincache_ingest::auth::Tokens,
    /// Rendered once at boot: it is a pure function of configuration.
    info: std::sync::Arc<str>,
    stats: crate::stats::Shards,
}

/// One request on one connection. Bundled so the handlers below take the exchange plus at
/// most the one decoded value their route carries.
pub struct Exchange<'a, 'head> {
    pub shard: usize,
    pub connection: &'a mut crate::connection::Connection,
    pub request: &'a crate::http::Request<'head>,
}

impl Cache {
    #[must_use]
    pub fn new(parts: Parts) -> Self {
        let Parts { ingest, tokens, info, stats } = parts;
        Self { ingest, tokens, info: info.render().into(), stats }
    }

    #[must_use]
    pub const fn stats(&self) -> &crate::stats::Shards {
        &self.stats
    }

    #[must_use]
    pub const fn ingest(&self) -> &bincache_ingest::ingest::Ingest {
        &self.ingest
    }

    /// Answers one request. The returned value decides whether the connection survives.
    pub async fn handle(
        &self,
        exchange: &mut Exchange<'_, '_>,
    ) -> Result<crate::http::KeepAlive, Error> {
        crate::stats::Shard::bump(&self.stats.get(exchange.shard).requests, 1);

        if exchange.request.framing == crate::http::Framing::Chunked {
            return self.answer(exchange, crate::http::response::Status::NotImplemented).await;
        }

        let route = match crate::route::resolve(exchange.request.target) {
            Ok(route) => route,
            Err(crate::route::Error::Unknown { .. }) => {
                return self.answer(exchange, crate::http::response::Status::NotFound).await;
            }
            Err(error) => {
                tracing::debug!(
                    target = exchange.request.target,
                    error = ?error,
                    "unroutable request"
                );
                return self.answer(exchange, crate::http::response::Status::BadRequest).await;
            }
        };

        match route {
            crate::route::Route::CacheInfo => self.cache_info(exchange).await,
            crate::route::Route::Metrics => self.metrics(exchange).await,
            crate::route::Route::Narinfo(key) => self.narinfo(exchange, key).await,
            crate::route::Route::Nar(url) => self.nar(exchange, &url).await,
        }
    }

    async fn cache_info(
        &self,
        exchange: &mut Exchange<'_, '_>,
    ) -> Result<crate::http::KeepAlive, Error> {
        let mut head = crate::http::response::Head::new(
            crate::http::response::Status::Ok,
            exchange.request.keep_alive,
        );
        head.header("Content-Type", CACHE_INFO_TYPE);
        let info = std::sync::Arc::clone(&self.info);
        self.send(exchange, head, info.as_bytes()).await
    }

    async fn metrics(
        &self,
        exchange: &mut Exchange<'_, '_>,
    ) -> Result<crate::http::KeepAlive, Error> {
        let paths = self.ingest.index().count().context(IndexSnafu)?;
        let body = self.stats.total().render(self.stats.len(), paths);
        let mut head = crate::http::response::Head::new(
            crate::http::response::Status::Ok,
            exchange.request.keep_alive,
        );
        head.header("Content-Type", METRICS_TYPE);
        self.send(exchange, head, body.as_bytes()).await
    }

    /// `GET` and `HEAD` render the same body; `HEAD` reuses its length and writes no body,
    /// which is the whole reason the stored artifact is a body rather than a framed
    /// response.
    async fn narinfo(
        &self,
        exchange: &mut Exchange<'_, '_>,
        key: bincache_core::storepath::Hash,
    ) -> Result<crate::http::KeepAlive, Error> {
        if exchange.request.method == crate::http::Method::Put {
            return self.publish(exchange).await;
        }

        let stats = self.stats.get(exchange.shard);
        let Some(record) = self.ingest.index().narinfo(&key).context(IndexSnafu)? else {
            crate::stats::Shard::bump(&stats.metadata_misses, 1);
            return self.answer(exchange, crate::http::response::Status::NotFound).await;
        };
        crate::stats::Shard::bump(&stats.metadata_hits, 1);

        let body = record.render(self.ingest.dir());
        let mut head = crate::http::response::Head::new(
            crate::http::response::Status::Ok,
            exchange.request.keep_alive,
        );
        head.header("Content-Type", bincache_core::narinfo::CONTENT_TYPE);
        self.send(exchange, head, body.as_bytes()).await
    }

    async fn nar(
        &self,
        exchange: &mut Exchange<'_, '_>,
        url: &bincache_core::narurl::NarUrl,
    ) -> Result<crate::http::KeepAlive, Error> {
        match exchange.request.method {
            crate::http::Method::Put => self.receive(exchange, url).await,
            crate::http::Method::Head => self.probe(exchange, url).await,
            crate::http::Method::Get => self.stream(exchange, url).await,
        }
    }

    /// The existence check `BinaryCacheStore::addToStore` runs before it uploads.
    ///
    /// A client that compressed nothing asks about `nar/<nar hash>.nar`, but bincache
    /// recompresses on receipt and stores the result under a different name, so this is
    /// answered from the NAR-hash index rather than from the filesystem. Answering `404`
    /// here would make every build node re-upload every NAR forever.
    async fn probe(
        &self,
        exchange: &mut Exchange<'_, '_>,
        url: &bincache_core::narurl::NarUrl,
    ) -> Result<crate::http::KeepAlive, Error> {
        let length = if url.compression == bincache_core::compression::Compression::None {
            self.ingest
                .index()
                .nar(&url.file_hash)
                .context(IndexSnafu)?
                .map(|entry| entry.nar_size.get())
        } else {
            let reader = self.ingest.store().read(url).await.context(StoreSnafu)?;
            reader.map(|reader| reader.size())
        };

        let Some(length) = length else {
            return self.answer(exchange, crate::http::response::Status::NotFound).await;
        };
        let mut head = crate::http::response::Head::new(
            crate::http::response::Status::Ok,
            exchange.request.keep_alive,
        );
        head.header("Content-Type", bincache_core::narurl::CONTENT_TYPE);
        head.header("Accept-Ranges", "bytes");
        head.length(length);
        exchange.connection.write(head.finish()).await.context(ConnectionSnafu)?;
        Ok(exchange.request.keep_alive)
    }

    async fn stream(
        &self,
        exchange: &mut Exchange<'_, '_>,
        url: &bincache_core::narurl::NarUrl,
    ) -> Result<crate::http::KeepAlive, Error> {
        let Some(reader) = self.ingest.store().read(url).await.context(StoreSnafu)? else {
            return self.answer(exchange, crate::http::response::Status::NotFound).await;
        };
        let size = reader.size();
        if size == 0 {
            // Ingest refuses an empty NAR, so a zero-length artifact means a truncated file
            // on disk rather than something a client did.
            tracing::error!(name = url.name(), "artifact on disk is empty");
            return self.answer(exchange, crate::http::response::Status::ServerError).await;
        }

        let span = match crate::range::resolve(exchange.request.range, size) {
            crate::range::Requested::Whole => crate::range::Span { first: 0, last: size - 1 },
            crate::range::Requested::Partial(span) => span,
            crate::range::Requested::Unsatisfiable => {
                let mut head = crate::http::response::Head::new(
                    crate::http::response::Status::RangeNotSatisfiable,
                    exchange.request.keep_alive,
                );
                head.header("Content-Range", format!("bytes */{size}"));
                head.header("Accept-Ranges", "bytes");
                head.length(0);
                exchange.connection.write(head.finish()).await.context(ConnectionSnafu)?;
                return Ok(exchange.request.keep_alive);
            }
        };
        let partial = span.len() != size;

        let status = if partial {
            crate::http::response::Status::PartialContent
        } else {
            crate::http::response::Status::Ok
        };
        let mut head = crate::http::response::Head::new(status, exchange.request.keep_alive);
        head.header("Content-Type", bincache_core::narurl::CONTENT_TYPE);
        head.header("Accept-Ranges", "bytes");
        if partial {
            head.header("Content-Range", format!("bytes {}-{}/{size}", span.first, span.last));
        }
        head.length(span.len());
        exchange.connection.write(head.finish()).await.context(ConnectionSnafu)?;

        let sent = self.pump(exchange, &reader, span).await?;
        crate::stats::Shard::bump(&self.stats.get(exchange.shard).payload_bytes, sent);
        Ok(exchange.request.keep_alive)
    }

    /// Bounded sends with the shard handed back between chunks, so one multi-gigabyte NAR
    /// cannot starve the metadata requests sharing its shard.
    ///
    /// One buffer serves the whole stream: it travels into the read, out of the write, and
    /// back, so a gigabyte transfer allocates once.
    async fn pump(
        &self,
        exchange: &mut Exchange<'_, '_>,
        reader: &bincache_store::nar::Reader,
        span: crate::range::Span,
    ) -> Result<u64, Error> {
        let mut offset = span.first;
        let mut buffer = Vec::with_capacity(crate::connection::SEND_CHUNK);
        while offset <= span.last {
            let wanted = core::cmp::min(
                span.last - offset + 1,
                u64::try_from(crate::connection::SEND_CHUNK).unwrap_or(u64::MAX),
            );
            buffer.clear();

            let (read, mut filled) = reader.read_at(buffer, offset).await.context(StoreSnafu)?;
            if read == 0 {
                tracing::error!(offset, "artifact ended before its recorded length");
                return Ok(offset - span.first);
            }
            filled.truncate(usize::try_from(wanted).unwrap_or(read));
            let sent = u64::try_from(filled.len()).unwrap_or(0);

            buffer = exchange.connection.write(filled).await.context(ConnectionSnafu)?;
            offset += sent;
            crate::connection::yield_now().await;
        }
        Ok(span.len())
    }

    /// `PUT nar/<nar hash>.nar`, the payload half of a push.
    async fn receive(
        &self,
        exchange: &mut Exchange<'_, '_>,
        url: &bincache_core::narurl::NarUrl,
    ) -> Result<crate::http::KeepAlive, Error> {
        if self.admit(exchange.shard, exchange.request) == bincache_ingest::auth::Admission::Denied
        {
            return self.answer(exchange, crate::http::response::Status::Unauthorized).await;
        }
        let crate::http::Framing::Length(mut remaining) = exchange.request.framing else {
            return self.answer(exchange, crate::http::response::Status::LengthRequired).await;
        };

        let mut upload = match self.ingest.receive(url).await {
            Ok(upload) => upload,
            Err(error) => {
                tracing::warn!(error = ?error, "refused an upload");
                return self.answer(exchange, crate::http::response::Status::BadRequest).await;
            }
        };
        self.proceed(exchange).await?;

        while remaining > 0 {
            let chunk = exchange.connection.chunk(&mut remaining).await.context(ConnectionSnafu)?;
            if chunk.is_empty() {
                break;
            }
            upload.write(&chunk).await.context(UploadSnafu)?;
            crate::connection::yield_now().await;
        }

        let compressed = upload.finish().await.context(UploadSnafu)?;
        let verified = match compressed.verify() {
            Ok(verified) => verified,
            Err(error) => {
                // The staging file goes away with the dropped upload; nothing durable
                // was ever named after unverified bytes.
                tracing::warn!(error = ?error, "upload failed verification");
                return self.answer(exchange, crate::http::response::Status::BadRequest).await;
            }
        };
        verified.store(self.ingest.store(), self.ingest.index()).await.context(UploadSnafu)?;

        crate::stats::Shard::bump(&self.stats.get(exchange.shard).uploads, 1);
        self.answer(exchange, crate::http::response::Status::Created).await
    }

    /// `PUT <hash>.narinfo`, the metadata half of a push, and the publish.
    async fn publish(
        &self,
        exchange: &mut Exchange<'_, '_>,
    ) -> Result<crate::http::KeepAlive, Error> {
        if self.admit(exchange.shard, exchange.request) == bincache_ingest::auth::Admission::Denied
        {
            return self.answer(exchange, crate::http::response::Status::Unauthorized).await;
        }
        let crate::http::Framing::Length(length) = exchange.request.framing else {
            return self.answer(exchange, crate::http::response::Status::LengthRequired).await;
        };
        if length > crate::http::METADATA_BODY_MAX {
            return self.answer(exchange, crate::http::response::Status::ContentTooLarge).await;
        }
        self.proceed(exchange).await?;

        let body = exchange.connection.body(length).await.context(ConnectionSnafu)?;
        let Ok(text) = String::from_utf8(body) else {
            return self.answer(exchange, crate::http::response::Status::BadRequest).await;
        };
        if let Err(error) = self.ingest.publish(&text) {
            tracing::warn!(error = ?error, "refused a narinfo");
            return self.answer(exchange, crate::http::response::Status::BadRequest).await;
        }
        self.answer(exchange, crate::http::response::Status::Created).await
    }

    /// Answers `Expect: 100-continue` once the request is known to be acceptable. curl sets
    /// it on uploads past about a kilobyte and stalls for a second if nothing answers.
    async fn proceed(&self, exchange: &mut Exchange<'_, '_>) -> Result<(), Error> {
        if exchange.request.expectation == crate::http::Expectation::Continue {
            let status = crate::http::response::Status::Continue;
            let head =
                format!("HTTP/1.1 {} {}\r\n\r\n", status.code(), status.reason()).into_bytes();
            exchange.connection.write(head).await.context(ConnectionSnafu)?;
        }
        Ok(())
    }

    fn admit(
        &self,
        shard: usize,
        request: &crate::http::Request<'_>,
    ) -> bincache_ingest::auth::Admission {
        let admission = request
            .authorization
            .and_then(bincache_ingest::auth::bearer)
            .map_or(bincache_ingest::auth::Admission::Denied, |token| self.tokens.admits(token));
        if admission == bincache_ingest::auth::Admission::Denied {
            crate::stats::Shard::bump(&self.stats.get(shard).rejections, 1);
        }
        admission
    }

    /// A bodyless answer. A request that announced a body it will not get to send has its
    /// connection closed, so the next request never starts mid-message.
    async fn answer(
        &self,
        exchange: &mut Exchange<'_, '_>,
        status: crate::http::response::Status,
    ) -> Result<crate::http::KeepAlive, Error> {
        let keep_alive = if exchange.request.body_len() > 0 {
            crate::http::KeepAlive::Close
        } else {
            exchange.request.keep_alive
        };
        let head = crate::http::response::bare(status, keep_alive, 0);
        exchange.connection.write(head).await.context(ConnectionSnafu)?;
        Ok(keep_alive)
    }

    async fn send(
        &self,
        exchange: &mut Exchange<'_, '_>,
        mut head: crate::http::response::Head,
        body: &[u8],
    ) -> Result<crate::http::KeepAlive, Error> {
        let bytes = if exchange.request.method == crate::http::Method::Head {
            head.length(u64::try_from(body.len()).unwrap_or(u64::MAX));
            head.finish()
        } else {
            head.with_body(body)
        };
        exchange.connection.write(bytes).await.context(ConnectionSnafu)?;
        Ok(exchange.request.keep_alive)
    }
}
