//! The write-side composition object: store, index, signing key, and store directory,
//! built once and threaded.
//!
//! It owns the two write operations a client performs and the two an operator performs.

use snafu::OptionExt as _;
use snafu::ResultExt as _;

#[derive(Debug, snafu::Snafu)]
#[snafu(visibility(pub))]
pub enum Error {
    #[snafu(display(
        "bincache receives uncompressed NARs and compresses them itself; this upload \
         declares {declared}. Point the client at the cache with `?compression=none`."
    ))]
    PreCompressed { declared: bincache_core::compression::Compression },

    #[snafu(display("the staging file could not be opened"))]
    Stage { source: bincache_store::nar::Error },

    #[snafu(display("the upload could not be started"))]
    Upload { source: crate::upload::Error },

    #[snafu(display("the narinfo body is malformed"))]
    Narinfo { source: bincache_core::narinfo::parse::Error },

    #[snafu(display(
        "no NAR with hash {nar_hash} has been uploaded, so there is nothing to publish"
    ))]
    UnknownNar { nar_hash: bincache_core::hash::Sha256 },

    #[snafu(display(
        "the narinfo declares NarSize {declared} but the uploaded NAR is {received} bytes"
    ))]
    NarSizeMismatch { declared: u64, received: u64 },

    #[snafu(display("the index rejected the write"))]
    Index { source: bincache_index::index::Error },
}

impl Error {
    /// Every variant that describes something the push itself got wrong is a client fault,
    /// and its message is written to be read by the person who ran `nix copy`.
    #[must_use]
    pub const fn fault(&self) -> crate::fault::Fault {
        match self {
            Self::PreCompressed { .. }
            | Self::Narinfo { .. }
            | Self::UnknownNar { .. }
            | Self::NarSizeMismatch { .. } => crate::fault::Fault::Client,
            Self::Stage { .. } | Self::Index { .. } => crate::fault::Fault::Server,
            Self::Upload { source } => source.fault(),
        }
    }
}

/// What the composition root assembles an [`Ingest`] from. A struct rather than five
/// positional arguments, so a call site cannot transpose two of them.
pub struct Parts {
    pub store: bincache_store::nar::Store,
    pub index: bincache_index::index::Index,
    pub key: bincache_core::sign::SecretKey,
    pub dir: bincache_core::storepath::Dir,
    pub level: crate::upload::Level,
}

/// Cheap to clone. Built at the composition root and shared by every shard.
#[derive(Clone)]
pub struct Ingest {
    store: bincache_store::nar::Store,
    index: bincache_index::index::Index,
    key: bincache_core::sign::SecretKey,
    dir: bincache_core::storepath::Dir,
    level: crate::upload::Level,
}

impl Ingest {
    #[must_use]
    pub fn new(parts: Parts) -> Self {
        let Parts { store, index, key, dir, level } = parts;
        Self { store, index, key, dir, level }
    }

    #[must_use]
    pub const fn index(&self) -> &bincache_index::index::Index {
        &self.index
    }

    #[must_use]
    pub const fn store(&self) -> &bincache_store::nar::Store {
        &self.store
    }

    #[must_use]
    pub const fn dir(&self) -> &bincache_core::storepath::Dir {
        &self.dir
    }

    /// Opens an upload for a `PUT nar/<hash>.nar`.
    ///
    /// The target must name an uncompressed NAR. bincache compresses on receipt so that the
    /// artifact it serves is zstd regardless of what the client can produce, and so that
    /// the NAR hash it verifies is over the bytes the protocol actually defines.
    pub async fn receive(
        &self,
        target: &bincache_core::narurl::NarUrl,
    ) -> Result<crate::upload::Upload<crate::upload::Receiving>, Error> {
        snafu::ensure!(
            target.compression == bincache_core::compression::Compression::None,
            PreCompressedSnafu { declared: target.compression }
        );
        let staged = self.store.stage().await.context(StageSnafu)?;
        crate::upload::Upload::new(staged, target.file_hash, self.level).context(UploadSnafu)
    }

    /// The publish, driven by a `PUT <hash>.narinfo`.
    ///
    /// Every field describing the payload is taken from what was actually received, not
    /// from what the body claims, and the client's own `Sig` lines are discarded: the key
    /// lives only here, so a compromised build node can poison only what it uploads.
    pub fn publish(&self, body: &str) -> Result<bincache_core::narinfo::NarInfo, Error> {
        let declared =
            bincache_core::narinfo::parse::parse(body, &self.dir).context(NarinfoSnafu)?;
        let entry = self
            .index
            .nar(&declared.nar_hash)
            .context(IndexSnafu)?
            .context(UnknownNarSnafu { nar_hash: declared.nar_hash })?;

        snafu::ensure!(
            entry.nar_size == declared.nar_size,
            NarSizeMismatchSnafu {
                declared: declared.nar_size.get(),
                received: entry.nar_size.get(),
            }
        );

        let mut record = bincache_core::narinfo::NarInfo {
            store_path: declared.store_path,
            compression: entry.compression,
            file_hash: entry.file_hash,
            file_size: entry.file_size,
            nar_hash: declared.nar_hash,
            nar_size: declared.nar_size,
            references: declared.references,
            deriver: declared.deriver,
            sigs: Vec::new(),
            ca: declared.ca,
        };
        record.resign(&self.dir, &self.key);

        self.index.publish(&record).context(IndexSnafu)?;
        tracing::info!(path = %record.store_path, "published");
        Ok(record)
    }
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    fn root(name: &str) -> std::path::PathBuf {
        let root = std::path::PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../../target"))
            .join("test-artifacts")
            .join("bincache-ingest-plane")
            .join(name);
        if let Err(error) = std::fs::remove_dir_all(&root) {
            assert_eq!(error.kind(), std::io::ErrorKind::NotFound, "stale root not removable");
        }
        root
    }

    async fn ingest(name: &str) -> crate::ingest::Ingest {
        let root = root(name);
        let store = bincache_store::nar::Store::open(root.join("payload")).await.expect("opens");
        let index = bincache_index::index::Index::open(root.join("index.redb")).expect("opens");
        crate::ingest::Ingest::new(crate::ingest::Parts {
            store,
            index,
            key: bincache_core::sign::SecretKey::generate("bincache-test-1".to_owned()),
            dir: bincache_core::storepath::Dir::new(
                bincache_core::storepath::DIR_DEFAULT.to_owned(),
            )
            .expect("absolute"),
            level: crate::upload::Level::new(3).expect("in range"),
        })
    }

    const PATH: &str = "5rnvz1n7hdmvbdzq0d5m5xrz3xz6ky8j-hello-2.12.1";

    /// What `nix copy --to 'http://host?compression=none'` renders and uploads.
    fn client_narinfo(body: &[u8]) -> String {
        let hash = bincache_core::hash::Sha256::digest(body);
        format!(
            "StorePath: /nix/store/{PATH}\n\
             URL: nar/{}.nar\n\
             Compression: none\n\
             FileHash: {hash}\n\
             FileSize: {}\n\
             NarHash: {hash}\n\
             NarSize: {}\n\
             References: \n\
             Sig: cache.example.org-1:{}\n",
            hash.base32(),
            body.len(),
            body.len(),
            data_encoding::BASE64.encode(&[7u8; 64]),
        )
    }

    async fn upload(ingest: &crate::ingest::Ingest, body: &[u8]) -> bincache_index::nar::Entry {
        let target = bincache_core::narurl::NarUrl {
            file_hash: bincache_core::hash::Sha256::digest(body),
            compression: bincache_core::compression::Compression::None,
        };
        let mut receiving = ingest.receive(&target).await.expect("receives");
        receiving.write(body).await.expect("writes");
        receiving
            .finish()
            .await
            .expect("finishes")
            .verify()
            .expect("verifies")
            .store(ingest.store(), ingest.index())
            .await
            .expect("stores")
    }

    #[compio::test]
    async fn publishes_a_record_that_describes_what_was_stored() {
        let ingest = ingest("publish").await;
        let body = b"nix-archive-1 body".repeat(40);
        let entry = upload(&ingest, &body).await;

        let record = ingest.publish(&client_narinfo(&body)).expect("publishes");
        assert_eq!(record.compression, bincache_core::compression::Compression::Zstd);
        assert_eq!(record.file_hash, entry.file_hash);
        assert_eq!(record.file_size, entry.file_size);
        assert_eq!(record.nar_hash, bincache_core::hash::Sha256::digest(&body));
        assert_eq!(record.nar_size.get(), u64::try_from(body.len()).expect("fits"));
        assert_eq!(record.store_path.to_string(), PATH);
    }

    /// Managed signing: the client's own `Sig` line is discarded and replaced by one from
    /// the key that never leaves this process.
    #[compio::test]
    async fn replaces_client_signatures_with_its_own() {
        let ingest = ingest("signing").await;
        let body = b"nix-archive-1 signed".repeat(40);
        upload(&ingest, &body).await;

        let record = ingest.publish(&client_narinfo(&body)).expect("publishes");
        assert_eq!(record.sigs.len(), 1);
        assert_eq!(record.sigs[0].name(), "bincache-test-1");
    }

    #[compio::test]
    async fn refuses_a_narinfo_whose_nar_was_never_uploaded() {
        let ingest = ingest("orphan-narinfo").await;
        let body = b"never uploaded";
        assert!(matches!(
            ingest.publish(&client_narinfo(body)),
            Err(crate::ingest::Error::UnknownNar { .. })
        ));
    }

    #[compio::test]
    async fn refuses_a_narinfo_that_lies_about_the_nar_size() {
        let ingest = ingest("size-lie").await;
        let body = b"nix-archive-1 truthful".repeat(10);
        upload(&ingest, &body).await;

        let honest = client_narinfo(&body);
        let lying = honest
            .replace(&format!("NarSize: {}", body.len()), &format!("NarSize: {}", body.len() + 1));
        assert!(matches!(
            ingest.publish(&lying),
            Err(crate::ingest::Error::NarSizeMismatch { .. })
        ));
    }

    #[compio::test]
    async fn refuses_a_pre_compressed_upload_and_names_the_fix() {
        let ingest = ingest("precompressed").await;
        let target = bincache_core::narurl::NarUrl {
            file_hash: bincache_core::hash::Sha256::digest(b"whatever"),
            compression: bincache_core::compression::Compression::Xz,
        };
        let refused = ingest.receive(&target).await;
        let message = match refused {
            Err(error) => error.to_string(),
            Ok(_) => panic!("an xz upload must be refused"),
        };
        assert!(message.contains("?compression=none"), "message was {message:?}");
    }

    #[compio::test]
    async fn delete_removes_both_the_record_and_the_artifact() {
        let ingest = ingest("delete").await;
        let body = b"nix-archive-1 deletable".repeat(10);
        upload(&ingest, &body).await;
        let record = ingest.publish(&client_narinfo(&body)).expect("publishes");

        let key = *record.store_path.hash();
        let store = ingest.store();
        let index = ingest.index();
        assert_eq!(
            crate::maintain::delete(store, index, &key).await.expect("deletes"),
            crate::maintain::Deleted::Removed
        );
        assert!(index.narinfo(&key).expect("reads").is_none());
        assert!(store.read(&record.nar()).await.expect("reads").is_none());
        assert_eq!(
            crate::maintain::delete(store, index, &key).await.expect("deletes"),
            crate::maintain::Deleted::Absent
        );
    }

    #[compio::test]
    async fn reconcile_sees_an_artifact_no_record_claims() {
        let ingest = ingest("reconcile").await;
        let body = b"nix-archive-1 unclaimed".repeat(10);
        upload(&ingest, &body).await;

        let reconciliation =
            crate::maintain::reconcile(ingest.store(), ingest.index()).expect("reconciles");
        assert_eq!(reconciliation.orphan_artifacts.len(), 1);
        assert!(reconciliation.records_without_artifacts.is_empty());

        ingest.publish(&client_narinfo(&body)).expect("publishes");
        let reconciliation =
            crate::maintain::reconcile(ingest.store(), ingest.index()).expect("reconciles");
        assert!(reconciliation.orphan_artifacts.is_empty());
        assert!(reconciliation.records_without_artifacts.is_empty());
    }

    #[compio::test]
    async fn rotate_re_signs_every_record_verifiably() {
        let ingest = ingest("rotate").await;
        let body = b"nix-archive-1 rotatable".repeat(10);
        upload(&ingest, &body).await;
        ingest.publish(&client_narinfo(&body)).expect("publishes");

        let key = bincache_core::sign::SecretKey::generate("bincache-test-2".to_owned());
        let rotated = crate::maintain::rotate(ingest.index(), ingest.dir(), &key).expect("rotates");
        assert_eq!(rotated, 1);
        for record in ingest.index().records().expect("lists") {
            assert_eq!(record.sigs.len(), 1);
            assert_eq!(record.sigs[0].name(), "bincache-test-2");
            key.public()
                .verify(&record.fingerprint(ingest.dir()), &record.sigs[0])
                .expect("verifies under the new key");
        }
    }
}
