//! The NAR upload pipeline, as a typestate machine.
//!
//! ```text
//! Upload<Receiving> ──▶ Upload<Compressed> ──▶ Upload<Verified> ──▶ receipt::Receipt
//!   stream + hash          encoder flushed        hash matched        fsync, rename,
//!   + compress             sizes final            the request target  write the receipt
//! ```
//!
//! Each transition consumes `self`, and `store` exists only on `Upload<Verified>`, so
//! committing unverified content is a compile error rather than a review catch.
//!
//! The verification target comes from the request URL: a client `PUT`s to
//! `nar/<nar hash>.nar`, so the request states what its own body must hash to. A mismatch
//! aborts before anything durable exists.

use snafu::OptionExt as _;
use snafu::ResultExt as _;
use std::io::Write as _;

/// Compressed output is drained into the staging file whenever the encoder has produced at
/// least this much, so peak memory is one chunk of compressed output rather than the whole
/// artifact.
const DRAIN_THRESHOLD: usize = 256 * 1024;

#[derive(Debug, snafu::Snafu)]
#[snafu(visibility(pub))]
pub enum Error {
    #[snafu(display("the staging file could not be written"))]
    Stage { source: bincache_store::nar::Error },

    #[snafu(display("zstd compression failed"))]
    Compress { source: std::io::Error },

    #[snafu(display("uploaded bytes hash to {found} but the request target declares {expected}"))]
    HashMismatch { expected: bincache_core::hash::Sha256, found: bincache_core::hash::Sha256 },

    #[snafu(display("an uploaded NAR must not be empty; nix rejects a zero NarSize"))]
    Empty,

    #[snafu(display("recording the stored artifact failed"))]
    Record { source: bincache_store::receipt::Error },
}

impl Error {
    /// A hash mismatch and an empty NAR are statements about the bytes the client sent.
    /// Everything else here is the staging file, the encoder, or the receipt failing.
    #[must_use]
    pub const fn fault(&self) -> crate::fault::Fault {
        match self {
            Self::HashMismatch { .. } | Self::Empty => crate::fault::Fault::Client,
            Self::Stage { .. } | Self::Compress { .. } | Self::Record { .. } => {
                crate::fault::Fault::Server
            }
        }
    }
}

/// A zstd compression level. Newtype so the valid range is checked once, at configuration
/// time, rather than trusted at every call.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Level(i32);

impl Level {
    /// `ZSTD_minCLevel` is negative and `ZSTD_maxCLevel` is 22. Negative levels trade ratio
    /// for speed and are legitimate for an ingest that wants to keep up with a build farm.
    pub const RANGE: core::ops::RangeInclusive<i32> = -7..=22;

    pub fn new(level: i32) -> Option<Self> {
        if Self::RANGE.contains(&level) { Some(Self(level)) } else { None }
    }

    #[must_use]
    pub const fn get(self) -> i32 {
        self.0
    }
}

/// Bytes arriving from the socket, being hashed and compressed on the way to staging.
/// Both digests run here: `nar` over what arrived, `file` over what is written out.
pub struct Receiving {
    encoder: zstd::stream::write::Encoder<'static, Vec<u8>>,
    nar: bincache_core::hash::Hasher,
    file: bincache_core::hash::Hasher,
    nar_size: u64,
}

/// The encoder is flushed, so both hashes and both sizes are final. Not yet checked.
#[derive(Debug)]
pub struct Compressed {
    nar_hash: bincache_core::hash::Sha256,
    nar_size: u64,
    file_hash: bincache_core::hash::Sha256,
    file_size: u64,
}

/// The received bytes hash to what the request target declared.
#[derive(Debug)]
pub struct Verified {
    nar_hash: bincache_core::hash::Sha256,
    nar_size: core::num::NonZeroU64,
    file_hash: bincache_core::hash::Sha256,
    file_size: u64,
}

pub struct Upload<S> {
    staged: bincache_store::nar::Staged,
    /// What the request target says the uncompressed body hashes to.
    expected: bincache_core::hash::Sha256,
    state: S,
}

impl Upload<Receiving> {
    /// `expected` is the hash read out of the `PUT` target, not out of the body.
    pub fn new(
        staged: bincache_store::nar::Staged,
        expected: bincache_core::hash::Sha256,
        level: Level,
    ) -> Result<Self, Error> {
        let encoder =
            zstd::stream::write::Encoder::new(Vec::new(), level.get()).context(CompressSnafu)?;
        let state = Receiving {
            encoder,
            nar: bincache_core::hash::Hasher::default(),
            file: bincache_core::hash::Hasher::default(),
            nar_size: 0,
        };
        Ok(Self { staged, expected, state })
    }

    /// Absorbs one chunk. The caller owns the loop, which awaits per chunk, so a large
    /// upload yields to its runtime rather than occupying a worker to completion.
    pub async fn write(&mut self, chunk: &[u8]) -> Result<(), Error> {
        self.state.nar.update(chunk);
        self.state.nar_size += u64::try_from(chunk.len()).unwrap_or(u64::MAX);
        self.state.encoder.write_all(chunk).context(CompressSnafu)?;
        if self.state.encoder.get_ref().len() >= DRAIN_THRESHOLD {
            self.drain().await?;
        }
        Ok(())
    }

    /// Flushes the encoder. After this the artifact's own hash is known, which is what
    /// names it on disk.
    pub async fn finish(mut self) -> Result<Upload<Compressed>, Error> {
        self.drain().await?;
        let Receiving { encoder, nar, mut file, nar_size } = self.state;

        let tail = encoder.finish().context(CompressSnafu)?;
        file.update(&tail);
        self.staged.write(&tail).await.context(StageSnafu)?;

        let state = Compressed {
            nar_hash: nar.finish(),
            nar_size,
            file_hash: file.finish(),
            file_size: self.staged.written(),
        };
        Ok(Upload { staged: self.staged, expected: self.expected, state })
    }

    /// Moves whatever the encoder has produced so far out of memory and into staging.
    async fn drain(&mut self) -> Result<(), Error> {
        let produced = core::mem::take(self.state.encoder.get_mut());
        if produced.is_empty() {
            return Ok(());
        }
        self.state.file.update(&produced);
        self.staged.write(&produced).await.context(StageSnafu)
    }
}

impl Upload<Compressed> {
    /// The gate. Nothing downstream can be reached without passing through here.
    pub fn verify(self) -> Result<Upload<Verified>, Error> {
        let Compressed { nar_hash, nar_size, file_hash, file_size } = self.state;
        snafu::ensure!(
            nar_hash == self.expected,
            HashMismatchSnafu { expected: self.expected, found: nar_hash }
        );
        let nar_size = core::num::NonZeroU64::new(nar_size).context(EmptySnafu)?;
        let state = Verified { nar_hash, nar_size, file_hash, file_size };
        Ok(Upload { staged: self.staged, expected: self.expected, state })
    }
}

impl Upload<Verified> {
    /// `fsync`, rename into the content-addressed name, then record where the NAR landed.
    ///
    /// The order matters: a crash between the two leaves an orphan artifact that
    /// reconciliation reports, never a receipt pointing at bytes that are not there.
    pub async fn store(
        self,
        store: &bincache_store::nar::Store,
        receipts: &bincache_store::receipt::Store,
    ) -> Result<bincache_store::receipt::Receipt, Error> {
        let Verified { nar_hash, nar_size, file_hash, file_size } = self.state;
        let url = bincache_core::narurl::NarUrl {
            file_hash,
            compression: bincache_core::compression::STORED,
        };

        self.staged.commit(store, &url).await.context(StageSnafu)?;

        let receipt = bincache_store::receipt::Receipt { file_hash, file_size, nar_size };
        receipts.write(&nar_hash, &receipt).await.context(RecordSnafu)?;
        Ok(receipt)
    }
}

/// Discards an upload that will not be verified. Available in every state, because a
/// dropped connection can happen at any point.
impl<S> Upload<S> {
    pub async fn abort(self) -> Result<(), Error> {
        self.staged.abort().await.context(StageSnafu)
    }
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    fn root(name: &str) -> std::path::PathBuf {
        let root = std::path::PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../../target"))
            .join("test-artifacts")
            .join("bincache-ingest")
            .join(name);
        if let Err(error) = std::fs::remove_dir_all(&root) {
            assert_eq!(error.kind(), std::io::ErrorKind::NotFound, "stale root not removable");
        }
        root
    }

    struct Harness {
        store: bincache_store::nar::Store,
        receipts: bincache_store::receipt::Store,
    }

    async fn harness(name: &str) -> Harness {
        let root = root(name);
        let store = bincache_store::nar::Store::open(root.clone()).await.expect("opens");
        let receipts = bincache_store::receipt::Store::open(&root).await.expect("opens");
        Harness { store, receipts }
    }

    fn level() -> crate::upload::Level {
        crate::upload::Level::new(3).expect("in range")
    }

    async fn receive(
        harness: &Harness,
        body: &[u8],
        expected: bincache_core::hash::Sha256,
    ) -> crate::upload::Upload<crate::upload::Compressed> {
        let staged = harness.store.stage().await.expect("stages");
        let mut upload = crate::upload::Upload::new(staged, expected, level()).expect("starts");
        for chunk in body.chunks(7) {
            upload.write(chunk).await.expect("writes");
        }
        upload.finish().await.expect("finishes")
    }

    #[tokio::test]
    async fn stores_and_records_a_verified_upload() {
        let harness = harness("verified").await;
        let body = b"nix-archive-1 pretend this is a real NAR".repeat(64);
        let nar_hash = bincache_core::hash::Sha256::digest(&body);

        let entry = receive(&harness, &body, nar_hash)
            .await
            .verify()
            .expect("verifies")
            .store(&harness.store, &harness.receipts)
            .await
            .expect("stores");

        assert_eq!(entry.nar_size.get(), u64::try_from(body.len()).expect("fits"));

        assert_eq!(harness.receipts.read(&nar_hash).expect("reads"), Some(entry));

        let reader = harness.store.read(&entry.url()).expect("reads").expect("present");
        assert_eq!(reader.size(), entry.file_size);
    }

    /// The stored artifact must decompress back to exactly what was uploaded, which is the
    /// property a client checks after it downloads.
    #[tokio::test]
    async fn the_stored_artifact_decompresses_to_the_uploaded_bytes() {
        let harness = harness("roundtrip").await;
        let body = b"nix-archive-1(type,regular,contents,".repeat(500);
        let nar_hash = bincache_core::hash::Sha256::digest(&body);

        let entry = receive(&harness, &body, nar_hash)
            .await
            .verify()
            .expect("verifies")
            .store(&harness.store, &harness.receipts)
            .await
            .expect("stores");

        let path = harness.store.path(&entry.url());
        let compressed = std::fs::read(path).expect("reads the artifact");
        let decompressed = zstd::stream::decode_all(compressed.as_slice()).expect("decompresses");
        assert_eq!(decompressed, body);
        assert_eq!(bincache_core::hash::Sha256::digest(&decompressed), nar_hash);
        assert_eq!(entry.file_hash, bincache_core::hash::Sha256::digest(&compressed));
    }

    #[tokio::test]
    async fn a_hash_mismatch_aborts_before_anything_durable_exists() {
        let harness = harness("mismatch").await;
        let body = b"the body that actually arrived";
        let declared = bincache_core::hash::Sha256::digest(b"what the client claimed");

        let upload = receive(&harness, body, declared).await;
        let refused = upload.verify();
        assert!(matches!(refused, Err(crate::upload::Error::HashMismatch { .. })));

        assert_eq!(harness.store.scan().expect("scans").artifacts.len(), 0);
        assert!(harness.receipts.read(&declared).expect("reads").is_none());
    }

    #[tokio::test]
    async fn an_empty_upload_is_refused() {
        let harness = harness("empty").await;
        let nar_hash = bincache_core::hash::Sha256::digest(b"");
        let upload = receive(&harness, b"", nar_hash).await;
        assert!(matches!(upload.verify(), Err(crate::upload::Error::Empty)));
    }

    #[tokio::test]
    async fn an_aborted_upload_leaves_no_staging_file() {
        let harness = harness("aborted").await;
        let staged = harness.store.stage().await.expect("stages");
        let expected = bincache_core::hash::Sha256::digest(b"");
        let mut upload = crate::upload::Upload::new(staged, expected, level()).expect("starts");
        upload.write(b"partial").await.expect("writes");
        upload.abort().await.expect("aborts");

        assert_eq!(harness.store.sweep_staging().await.expect("sweeps"), 0);
    }

    #[test]
    fn compression_levels_are_range_checked() {
        assert!(crate::upload::Level::new(3).is_some());
        assert!(crate::upload::Level::new(22).is_some());
        assert!(crate::upload::Level::new(23).is_none());
        assert!(crate::upload::Level::new(-8).is_none());
    }

    /// Uploading the same NAR twice must be a no-op rather than a conflict: content
    /// addressing means both writers produced identical bytes.
    #[tokio::test]
    async fn re_uploading_the_same_nar_is_idempotent() {
        let harness = harness("idempotent").await;
        let body = b"nix-archive-1 repeated".repeat(32);
        let nar_hash = bincache_core::hash::Sha256::digest(&body);

        let mut entries = Vec::new();
        for _ in 0..2 {
            let entry = receive(&harness, &body, nar_hash)
                .await
                .verify()
                .expect("verifies")
                .store(&harness.store, &harness.receipts)
                .await
                .expect("stores");
            entries.push(entry);
        }
        assert_eq!(entries[0], entries[1]);
        assert_eq!(harness.store.scan().expect("scans").artifacts.len(), 1);
    }
}
