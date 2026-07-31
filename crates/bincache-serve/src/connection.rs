//! One keep-alive connection: the read buffer, the head/body split, and the writes.
//!
//! The buffer is per-connection and reused across requests on it. The design's registered
//! per-shard pool with stable addresses is the named upgrade; it is not built here because
//! it would be an unmeasured optimization, which `research/DESIGN_V2.md` calls a bug in the
//! charter.

use snafu::ResultExt as _;

/// Bytes requested per socket read. Large enough that a NAR upload is not syscall-bound,
/// small enough that ten thousand idle connections are not a memory problem.
pub const READ_CHUNK: usize = 64 * 1024;

/// Bytes sent per payload chunk. Bounding this is what stops one elephant stream from
/// monopolizing its shard: the shard re-enters the scheduler between chunks.
pub const SEND_CHUNK: usize = 256 * 1024;

#[derive(Debug, snafu::Snafu)]
#[snafu(visibility(pub))]
pub enum Error {
    #[snafu(display("reading from the socket failed"))]
    Read { source: std::io::Error },

    #[snafu(display("writing to the socket failed"))]
    Write { source: std::io::Error },

    #[snafu(display("the peer closed the connection mid-message"))]
    Truncated,

    #[snafu(display("the request head exceeds {} bytes", crate::http::HEAD_LEN_MAX))]
    HeadTooLong,
}

pub struct Connection {
    stream: compio::net::TcpStream,
    /// Bytes read from the socket that no request has consumed yet.
    buffer: Vec<u8>,
}

impl Connection {
    #[must_use]
    pub fn new(stream: compio::net::TcpStream) -> Self {
        Self { stream, buffer: Vec::with_capacity(READ_CHUNK) }
    }

    /// Reads until a complete request head is buffered, and hands it back as an owned copy
    /// so the borrow of the connection ends here.
    ///
    /// `None` means the peer closed the connection between requests, which is the ordinary
    /// end of a keep-alive session and not a fault.
    pub async fn head(&mut self) -> Result<Option<Vec<u8>>, Error> {
        loop {
            if let Some(len) = crate::http::head_len(&self.buffer) {
                let head = self.buffer[..len].to_vec();
                self.buffer.drain(..len);
                return Ok(Some(head));
            }
            snafu::ensure!(self.buffer.len() <= crate::http::HEAD_LEN_MAX, HeadTooLongSnafu);

            let read = self.fill().await?;
            if read == 0 {
                snafu::ensure!(self.buffer.is_empty(), TruncatedSnafu);
                return Ok(None);
            }
        }
    }

    /// A whole small body. Callers bound `len` before calling; this is for narinfo bodies,
    /// never for NAR payloads.
    pub async fn body(&mut self, len: u64) -> Result<Vec<u8>, Error> {
        let len = usize::try_from(len).unwrap_or(usize::MAX);
        while self.buffer.len() < len {
            let read = self.fill().await?;
            snafu::ensure!(read > 0, TruncatedSnafu);
        }
        let body = self.buffer[..len].to_vec();
        self.buffer.drain(..len);
        Ok(body)
    }

    /// The next slice of a streamed body, at most [`READ_CHUNK`] bytes. `remaining` is
    /// decremented by what is returned, so the caller's loop is bounded by the declared
    /// `Content-Length` and cannot run past it.
    pub async fn chunk(&mut self, remaining: &mut u64) -> Result<Vec<u8>, Error> {
        if *remaining == 0 {
            return Ok(Vec::new());
        }
        if self.buffer.is_empty() {
            let read = self.fill().await?;
            snafu::ensure!(read > 0, TruncatedSnafu);
        }
        let wanted = usize::try_from(*remaining).unwrap_or(usize::MAX);
        let take = core::cmp::min(core::cmp::min(self.buffer.len(), wanted), READ_CHUNK);
        let chunk = self.buffer[..take].to_vec();
        self.buffer.drain(..take);
        *remaining -= u64::try_from(take).unwrap_or(0);
        Ok(chunk)
    }

    /// Reads and throws away a body the handler will not process, so the next request on a
    /// kept-alive connection starts at a message boundary.
    pub async fn discard(&mut self, mut remaining: u64) -> Result<(), Error> {
        while remaining > 0 {
            let chunk = self.chunk(&mut remaining).await?;
            snafu::ensure!(!chunk.is_empty(), TruncatedSnafu);
        }
        Ok(())
    }

    /// Writes everything, handing the buffer back so a streaming caller can reuse one
    /// allocation for a whole transfer.
    pub async fn write(&mut self, bytes: Vec<u8>) -> Result<Vec<u8>, Error> {
        let compio::BufResult(result, bytes) =
            compio::io::AsyncWriteExt::write_all(&mut self.stream, bytes).await;
        result.context(WriteSnafu)?;
        Ok(bytes)
    }

    /// Appends into the buffer's spare capacity, returning how many bytes arrived. Zero
    /// means the peer half-closed.
    async fn fill(&mut self) -> Result<usize, Error> {
        let len = self.buffer.len();
        self.buffer.reserve(READ_CHUNK);
        let buffer = core::mem::take(&mut self.buffer);
        let compio::BufResult(result, slice) =
            compio::io::AsyncRead::read(&mut self.stream, compio::buf::IoBuf::slice(buffer, len..))
                .await;
        self.buffer = compio::buf::IntoInner::into_inner(slice);
        result.context(ReadSnafu)
    }
}

/// Hands the shard back to its scheduler once.
///
/// Every completion-based read and write already re-enters the scheduler, so this is only
/// needed where a loop does CPU work between I/O operations: compressing an upload, and
/// sending a payload whose chunks are already in the page cache. Keeping it explicit is
/// what makes "bounded work per period" reviewable rather than incidental.
pub async fn yield_now() {
    Yield { yielded: false }.await;
}

struct Yield {
    yielded: bool,
}

impl core::future::Future for Yield {
    type Output = ();

    fn poll(
        mut self: core::pin::Pin<&mut Self>,
        context: &mut core::task::Context<'_>,
    ) -> core::task::Poll<Self::Output> {
        if self.yielded {
            core::task::Poll::Ready(())
        } else {
            self.yielded = true;
            context.waker().wake_by_ref();
            core::task::Poll::Pending
        }
    }
}
