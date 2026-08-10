//! The write-side composition object: the three data directories, the signing key, and the
//! store directory, built once and threaded.
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

    #[snafu(display("the NAR receipt could not be read"))]
    Receipt { source: bincache_store::receipt::Error },

    #[snafu(display("the narinfo could not be published"))]
    Publish { source: bincache_store::narinfo::Error },
}

impl Error {
    /// Every variant that describes something the push itself got wrong is a client fault,
    /// and its message is written to be read by the person who ran `nix copy`.
    #[must_use]
    pub const fn fault(&self) -> crate::fault::Fault {
        match self {
            Self::PreCompressed { .. } | Self::Narinfo { .. } | Self::UnknownNar { .. } => {
                crate::fault::Fault::Client
            }
            Self::Stage { .. } | Self::Receipt { .. } | Self::Publish { .. } => {
                crate::fault::Fault::Server
            }
            Self::Upload { source } => source.fault(),
        }
    }
}

/// What the composition root assembles an [`Ingest`] from. A struct rather than five
/// positional arguments, so a call site cannot transpose two of them.
pub struct Parts {
    pub store: bincache_store::nar::Store,
    pub narinfo: bincache_store::narinfo::Store,
    pub receipt: bincache_store::receipt::Store,
    pub key: bincache_core::sign::SecretKey,
    pub dir: bincache_core::storepath::Dir,
    pub level: crate::upload::Level,
}

/// Cheap to clone. Built at the composition root and shared by every request.
#[derive(Clone)]
pub struct Ingest {
    store: bincache_store::nar::Store,
    narinfo: bincache_store::narinfo::Store,
    receipt: bincache_store::receipt::Store,
    key: bincache_core::sign::SecretKey,
    dir: bincache_core::storepath::Dir,
    level: crate::upload::Level,
    /// How many paths are published, seeded by one directory walk at construction.
    ///
    /// ponytail: an out-of-band `bincache delete` while the server runs makes this stale
    /// until the next restart. Recounting per scrape would be a directory walk on a
    /// million files; a gauge that is right at boot and right for every publish is worth
    /// more than one that is exact and slow.
    paths: std::sync::Arc<core::sync::atomic::AtomicU64>,
}

impl Ingest {
    pub fn new(parts: Parts) -> Result<Self, Error> {
        let Parts { store, narinfo, receipt, key, dir, level } = parts;
        let counted = narinfo.count().context(PublishSnafu)?;
        let paths = std::sync::Arc::new(core::sync::atomic::AtomicU64::new(counted));
        Ok(Self { store, narinfo, receipt, key, dir, level, paths })
    }

    #[must_use]
    pub const fn narinfo(&self) -> &bincache_store::narinfo::Store {
        &self.narinfo
    }

    #[must_use]
    pub const fn receipt(&self) -> &bincache_store::receipt::Store {
        &self.receipt
    }

    #[must_use]
    pub fn paths(&self) -> u64 {
        self.paths.load(core::sync::atomic::Ordering::Relaxed)
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
    /// `NarSize` comes from the receipt rather than from the body, like every other field
    /// describing the payload. A client that declares a different one is not refused: the
    /// hash it declared already determines the content, which determines the size, so the
    /// received value is authoritative and the declared one is noise.
    pub async fn publish(&self, body: &str) -> Result<bincache_core::narinfo::NarInfo, Error> {
        let declared =
            bincache_core::narinfo::parse::parse(body, &self.dir).context(NarinfoSnafu)?;
        let receipt = self
            .receipt
            .read(&declared.nar_hash)
            .context(ReceiptSnafu)?
            .context(UnknownNarSnafu { nar_hash: declared.nar_hash })?;

        let mut record = bincache_core::narinfo::NarInfo {
            store_path: declared.store_path,
            compression: bincache_core::compression::STORED,
            file_hash: receipt.file_hash,
            file_size: receipt.file_size,
            nar_hash: declared.nar_hash,
            nar_size: receipt.nar_size,
            references: declared.references,
            deriver: declared.deriver,
            sigs: Vec::new(),
            ca: declared.ca,
        };
        record.resign(&self.dir, &self.key);

        let wrote = self
            .narinfo
            .write(record.store_path.hash(), &record.render(&self.dir))
            .await
            .context(PublishSnafu)?;
        if wrote == bincache_store::atomic::Wrote::Created {
            self.paths.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        }
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
        let store = bincache_store::nar::Store::open(root.clone()).await.expect("opens");
        let narinfo = bincache_store::narinfo::Store::open(&root).await.expect("opens");
        let receipt = bincache_store::receipt::Store::open(&root).await.expect("opens");
        crate::ingest::Ingest::new(crate::ingest::Parts {
            store,
            narinfo,
            receipt,
            key: bincache_core::sign::SecretKey::generate("bincache-test-1".to_owned()),
            dir: bincache_core::storepath::Dir::new(
                bincache_core::storepath::DIR_DEFAULT.to_owned(),
            )
            .expect("absolute"),
            level: crate::upload::Level::new(3).expect("in range"),
        })
        .expect("counts what is published")
    }

    const PATH: &str = "5rnvz1n7hdmvbdzq0d5m5xrz3xz6ky8j-hello-2.12.1";

    /// A second store path with different contents in its *name* only. A NAR does not
    /// include the name, so both paths hash to the same NAR and share one artifact.
    const SIBLING: &str = "9xkzq1n7hdmvbdzq0d5m5xrz3xz6ky8j-hello-2.12.1";

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

    async fn upload(
        ingest: &crate::ingest::Ingest,
        body: &[u8],
    ) -> bincache_store::receipt::Receipt {
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
            .store(ingest.store(), ingest.receipt())
            .await
            .expect("stores")
    }

    #[tokio::test]
    async fn publishes_a_record_that_describes_what_was_stored() {
        let ingest = ingest("publish").await;
        let body = b"nix-archive-1 body".repeat(40);
        let entry = upload(&ingest, &body).await;

        let record = ingest.publish(&client_narinfo(&body)).await.expect("publishes");
        assert_eq!(record.compression, bincache_core::compression::Compression::Zstd);
        assert_eq!(record.file_hash, entry.file_hash);
        assert_eq!(record.file_size, entry.file_size);
        assert_eq!(record.nar_hash, bincache_core::hash::Sha256::digest(&body));
        assert_eq!(record.nar_size.get(), u64::try_from(body.len()).expect("fits"));
        assert_eq!(record.store_path.to_string(), PATH);
    }

    /// Managed signing: the client's own `Sig` line is discarded and replaced by one from
    /// the key that never leaves this process.
    #[tokio::test]
    async fn replaces_client_signatures_with_its_own() {
        let ingest = ingest("signing").await;
        let body = b"nix-archive-1 signed".repeat(40);
        upload(&ingest, &body).await;

        let record = ingest.publish(&client_narinfo(&body)).await.expect("publishes");
        assert_eq!(record.sigs.len(), 1);
        assert_eq!(record.sigs[0].name(), "bincache-test-1");
    }

    #[tokio::test]
    async fn refuses_a_narinfo_whose_nar_was_never_uploaded() {
        let ingest = ingest("orphan-narinfo").await;
        let body = b"never uploaded";
        assert!(matches!(
            ingest.publish(&client_narinfo(body)).await,
            Err(crate::ingest::Error::UnknownNar { .. })
        ));
    }

    /// A client that declares the wrong `NarSize` is corrected, not refused.
    ///
    /// The `NarHash` it declared already determines the content, which determines the size,
    /// so the received value is authoritative. Publishing the received one keeps the rule
    /// that every field describing the payload comes from what arrived.
    #[tokio::test]
    async fn a_narinfo_that_lies_about_the_nar_size_is_corrected() {
        let ingest = ingest("size-lie").await;
        let body = b"nix-archive-1 truthful".repeat(10);
        upload(&ingest, &body).await;

        let honest = client_narinfo(&body);
        let lying = honest
            .replace(&format!("NarSize: {}", body.len()), &format!("NarSize: {}", body.len() + 1));
        let record = ingest.publish(&lying).await.expect("publishes");
        assert_eq!(record.nar_size.get(), u64::try_from(body.len()).expect("fits"));
    }

    #[tokio::test]
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

    #[tokio::test]
    async fn delete_forgets_the_record_and_leaves_the_artifact() {
        let ingest = ingest("delete").await;
        let body = b"nix-archive-1 deletable".repeat(10);
        upload(&ingest, &body).await;
        let record = ingest.publish(&client_narinfo(&body)).await.expect("publishes");

        let key = *record.store_path.hash();
        let (narinfo, receipts, dir) = (ingest.narinfo(), ingest.receipt(), ingest.dir());
        assert_eq!(
            crate::maintain::delete(narinfo, receipts, dir, &key).await.expect("deletes"),
            crate::maintain::Deleted::Removed
        );
        assert!(narinfo.read(&key).expect("reads").is_none());
        assert!(receipts.read(&record.nar_hash).expect("reads").is_none());

        // The artifact stays. Whether anything still needs it is reconcile's question.
        assert!(ingest.store().read(&record.nar()).expect("reads").is_some());
        assert_eq!(
            crate::maintain::delete(narinfo, receipts, dir, &key).await.expect("deletes"),
            crate::maintain::Deleted::Absent
        );
    }

    /// A NAR excludes the store path name, so two paths whose contents are identical share
    /// one artifact. Deleting one must not strand the other.
    #[tokio::test]
    async fn deleting_one_path_leaves_a_sibling_that_shares_its_nar_servable() {
        let ingest = ingest("shared-nar").await;
        let body = b"nix-archive-1 shared".repeat(10);
        upload(&ingest, &body).await;

        // Two store paths, same contents, therefore the same NarHash and the same artifact.
        let first = ingest.publish(&client_narinfo(&body)).await.expect("publishes");
        let second_body = client_narinfo(&body).replace(PATH, SIBLING);
        let second = ingest.publish(&second_body).await.expect("publishes");
        assert_eq!(first.nar_hash, second.nar_hash);
        assert_eq!(first.file_hash, second.file_hash);

        let key = *first.store_path.hash();
        crate::maintain::delete(ingest.narinfo(), ingest.receipt(), ingest.dir(), &key)
            .await
            .expect("deletes");

        let survivor = ingest
            .narinfo()
            .read(second.store_path.hash())
            .expect("reads")
            .expect("the sibling is still published");
        assert!(!survivor.is_empty());
        assert!(
            ingest.store().read(&second.nar()).expect("reads").is_some(),
            "the sibling's artifact must survive the other path's delete"
        );
    }

    #[tokio::test]
    async fn reconcile_sees_an_artifact_no_record_claims() {
        let ingest = ingest("reconcile").await;
        let body = b"nix-archive-1 unclaimed".repeat(10);
        upload(&ingest, &body).await;

        let reconciliation =
            crate::maintain::reconcile(ingest.store(), ingest.narinfo(), ingest.dir())
                .await
                .expect("reconciles");
        assert_eq!(reconciliation.orphan_artifacts.len(), 1);
        assert!(reconciliation.records_without_artifacts.is_empty());

        ingest.publish(&client_narinfo(&body)).await.expect("publishes");
        let reconciliation =
            crate::maintain::reconcile(ingest.store(), ingest.narinfo(), ingest.dir())
                .await
                .expect("reconciles");
        assert!(reconciliation.orphan_artifacts.is_empty());
        assert!(reconciliation.records_without_artifacts.is_empty());
    }

    /// The artifact a delete leaves behind is exactly what reconcile is for.
    #[tokio::test]
    async fn reconcile_reports_what_a_delete_left_behind() {
        let ingest = ingest("reconcile-after-delete").await;
        let body = b"nix-archive-1 leftover".repeat(10);
        upload(&ingest, &body).await;
        let record = ingest.publish(&client_narinfo(&body)).await.expect("publishes");

        crate::maintain::delete(
            ingest.narinfo(),
            ingest.receipt(),
            ingest.dir(),
            record.store_path.hash(),
        )
        .await
        .expect("deletes");

        let reconciliation =
            crate::maintain::reconcile(ingest.store(), ingest.narinfo(), ingest.dir())
                .await
                .expect("reconciles");
        assert_eq!(reconciliation.orphan_artifacts, vec![record.nar()]);
        assert!(reconciliation.records_without_artifacts.is_empty());
    }

    #[tokio::test]
    async fn rotate_re_signs_every_record_verifiably() {
        let ingest = ingest("rotate").await;
        let body = b"nix-archive-1 rotatable".repeat(10);
        upload(&ingest, &body).await;
        ingest.publish(&client_narinfo(&body)).await.expect("publishes");

        let key = bincache_core::sign::SecretKey::generate("bincache-test-2".to_owned());
        let rotated =
            crate::maintain::rotate(ingest.narinfo(), ingest.dir(), &key).await.expect("rotates");
        assert_eq!(rotated, 1);

        let published = ingest
            .narinfo()
            .read(bincache_core::storepath::Path::parse(PATH).expect("parses").hash())
            .expect("reads")
            .expect("present");
        let body = String::from_utf8(published).expect("utf8");
        let record = bincache_core::narinfo::parse::parse(&body, ingest.dir()).expect("parses");
        assert_eq!(record.sigs.len(), 1);
        assert_eq!(record.sigs[0].name(), "bincache-test-2");
        key.public()
            .verify(&record.fingerprint(ingest.dir()), &record.sigs[0])
            .expect("verifies under the new key");
    }
}
