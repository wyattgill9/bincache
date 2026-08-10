//! Operator maintenance: delete, reconcile, and signing-key rotation.
//!
//! Separate from [`crate::ingest::Ingest`] because the inputs are genuinely different. A
//! delete needs no signing key and no compression level, and folding it into the serving
//! object would mean every caller had to invent values it never uses.
//!
//! Nothing here runs on the serving path.

use snafu::ResultExt as _;

/// How often a rotation pass reports progress. Rotation rewrites every record, so a large
/// cache needs to look alive rather than hung.
const PROGRESS: usize = 1000;

#[derive(Debug, snafu::Snafu)]
#[snafu(visibility(pub))]
pub enum Error {
    #[snafu(display("the narinfo directory could not be used"))]
    Narinfo { source: bincache_store::narinfo::Error },

    #[snafu(display("the receipt directory could not be used"))]
    Receipt { source: bincache_store::receipt::Error },

    #[snafu(display("the artifact could not be unlinked"))]
    Unlink { source: bincache_store::nar::Error },

    #[snafu(display("the payload directory could not be listed"))]
    Scan { source: bincache_store::nar::Error },

    #[snafu(display(
        "published narinfo {key} does not parse; the cache wrote it, so this is \
                     corruption rather than bad input"
    ))]
    Corrupt { key: bincache_core::storepath::Hash, source: bincache_core::narinfo::parse::Error },

    #[snafu(display("published narinfo {key} is not UTF-8"))]
    Encoding { key: bincache_core::storepath::Hash, source: std::string::FromUtf8Error },

    #[snafu(display("published narinfo {key} vanished mid-pass"))]
    Vanished { key: bincache_core::storepath::Hash },
}

/// Whether a delete found anything.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Deleted {
    Removed,
    Absent,
}

/// What a reconciliation pass found. Artifacts with no record are deletable; records with
/// no artifact are the louder problem, because a client that resolved the narinfo will ask
/// for bytes that are not there.
#[derive(Debug, Default)]
pub struct Reconciliation {
    pub orphan_artifacts: Vec<bincache_core::narurl::NarUrl>,
    pub records_without_artifacts: Vec<bincache_core::storepath::Path>,
    pub unrecognized_files: Vec<std::path::PathBuf>,
}

/// Forget the record and its receipt. The artifact stays.
///
/// Leaving it is deliberate. A NAR excludes the store path name, so two paths with
/// identical contents produce the same `NarHash`, the same `FileHash`, and therefore the
/// same artifact. Unlinking here would strand the sibling's narinfo pointing at bytes that
/// are gone. The now-unreferenced artifact shows up in [`reconcile`], which is where the
/// question "is anything still using this?" can actually be answered.
///
/// It also makes a delete O(1) rather than a walk of every record.
pub async fn delete(
    narinfo: &bincache_store::narinfo::Store,
    receipts: &bincache_store::receipt::Store,
    dir: &bincache_core::storepath::Dir,
    key: &bincache_core::storepath::Hash,
) -> Result<Deleted, Error> {
    let Some(record) = read(narinfo, dir, key).await? else {
        return Ok(Deleted::Absent);
    };
    receipts.remove(&record.nar_hash).await.context(ReceiptSnafu)?;
    narinfo.remove(key).await.context(NarinfoSnafu)?;
    tracing::info!(path = %record.store_path, "deleted");
    Ok(Deleted::Removed)
}

/// Compares the filesystem against the published records in both directions. Reports rather
/// than acts, because deleting is an operator decision.
pub async fn reconcile(
    store: &bincache_store::nar::Store,
    narinfo: &bincache_store::narinfo::Store,
    dir: &bincache_core::storepath::Dir,
) -> Result<Reconciliation, Error> {
    let scan = store.scan().context(ScanSnafu)?;
    let records = records(narinfo, dir).await?;

    let claimed: std::collections::HashSet<String> =
        records.iter().map(|record| record.nar().name()).collect();
    let present: std::collections::HashSet<String> =
        scan.artifacts.iter().map(bincache_core::narurl::NarUrl::name).collect();

    let orphan_artifacts =
        scan.artifacts.iter().filter(|url| !claimed.contains(&url.name())).copied().collect();
    let records_without_artifacts = records
        .into_iter()
        .filter(|record| !present.contains(&record.nar().name()))
        .map(|record| record.store_path)
        .collect();

    Ok(Reconciliation {
        orphan_artifacts,
        records_without_artifacts,
        unrecognized_files: scan.unrecognized,
    })
}

/// Re-signs every record under `key`. Clients see no interruption as long as both public
/// keys sit in `trusted-public-keys` for the duration, which is what makes rotation a
/// background pass rather than an outage.
///
/// One rewrite per record, each its own atomic rename. There is no shared transaction to
/// grow, so the cost is linear in records rather than in records times index depth, and an
/// interrupted rotation leaves a mix of old and new signatures that a rerun finishes.
pub async fn rotate(
    narinfo: &bincache_store::narinfo::Store,
    dir: &bincache_core::storepath::Dir,
    key: &bincache_core::sign::SecretKey,
) -> Result<usize, Error> {
    let keys = narinfo.keys().context(NarinfoSnafu)?;
    let total = keys.len();
    let mut rotated = 0;
    for published in keys {
        let Some(mut record) = read(narinfo, dir, &published).await? else {
            // Deleted between the listing and now. Not a failure: the record it would have
            // re-signed no longer exists.
            continue;
        };
        record.resign(dir, key);
        narinfo.write(&published, &record.render(dir)).await.context(NarinfoSnafu)?;
        rotated += 1;
        if rotated % PROGRESS == 0 {
            tracing::info!(rotated, total, "re-signing");
        }
    }
    Ok(rotated)
}

/// Every published record, parsed back from the bytes being served.
async fn records(
    narinfo: &bincache_store::narinfo::Store,
    dir: &bincache_core::storepath::Dir,
) -> Result<Vec<bincache_core::narinfo::NarInfo>, Error> {
    let keys = narinfo.keys().context(NarinfoSnafu)?;
    let mut records = Vec::with_capacity(keys.len());
    for key in keys {
        // A record that vanished between the listing and the read was deleted underneath
        // this pass, which is not a fault in what remains.
        if let Some(record) = read(narinfo, dir, &key).await? {
            records.push(record);
        }
    }
    Ok(records)
}

/// Reads one published record back.
///
/// A parse failure here is corruption rather than bad input: this cache rendered and wrote
/// these bytes itself. The one operator mistake it also catches is a `--store-dir` that
/// does not match the one these records were published under, which fails loudly on the
/// `StorePath:` prefix instead of silently rewriting every record into the wrong store.
async fn read(
    narinfo: &bincache_store::narinfo::Store,
    dir: &bincache_core::storepath::Dir,
    key: &bincache_core::storepath::Hash,
) -> Result<Option<bincache_core::narinfo::NarInfo>, Error> {
    let Some(raw) = narinfo.read(key).await.context(NarinfoSnafu)? else {
        return Ok(None);
    };
    let body = String::from_utf8(raw).context(EncodingSnafu { key: *key })?;
    let record =
        bincache_core::narinfo::parse::parse(&body, dir).context(CorruptSnafu { key: *key })?;
    Ok(Some(record))
}
