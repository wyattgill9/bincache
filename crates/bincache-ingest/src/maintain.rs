//! Operator maintenance: delete, reconcile, and signing-key rotation.
//!
//! Separate from [`crate::ingest::Ingest`] because the inputs are genuinely different. A
//! delete needs no signing key and no compression level, and folding it into the serving
//! object would mean every caller had to invent values it never uses.
//!
//! Nothing here runs on the serving path.

use snafu::ResultExt as _;

#[derive(Debug, snafu::Snafu)]
#[snafu(visibility(pub))]
pub enum Error {
    #[snafu(display("the index rejected the operation"))]
    Index { source: bincache_index::index::Error },

    #[snafu(display("the artifact could not be unlinked"))]
    Unlink { source: bincache_store::nar::Error },

    #[snafu(display("the payload directory could not be listed"))]
    Scan { source: bincache_store::nar::Error },
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

/// Forget the record, then unlink the artifact.
///
/// That order is what makes an interrupted delete safe: it can leave an orphan artifact,
/// which reconciliation collects, but never a record pointing at bytes that are gone. An
/// in-flight stream holds its descriptor, so an unlinked file finishes streaming.
pub async fn delete(
    store: &bincache_store::nar::Store,
    index: &bincache_index::index::Index,
    key: &bincache_core::storepath::Hash,
) -> Result<Deleted, Error> {
    let Some(record) = index.unpublish(key).context(IndexSnafu)? else {
        return Ok(Deleted::Absent);
    };
    store.remove(&record.nar()).await.context(UnlinkSnafu)?;
    tracing::info!(path = %record.store_path, "deleted");
    Ok(Deleted::Removed)
}

/// Compares the filesystem against the index in both directions. Reports rather than acts,
/// because deleting is an operator decision.
pub fn reconcile(
    store: &bincache_store::nar::Store,
    index: &bincache_index::index::Index,
) -> Result<Reconciliation, Error> {
    let scan = store.scan().context(ScanSnafu)?;
    let records = index.records().context(IndexSnafu)?;

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
pub fn rotate(
    index: &bincache_index::index::Index,
    dir: &bincache_core::storepath::Dir,
    key: &bincache_core::sign::SecretKey,
) -> Result<usize, Error> {
    let records = index.records().context(IndexSnafu)?;
    let rotated = records.len();
    for mut record in records {
        record.resign(dir, key);
        index.publish(&record).context(IndexSnafu)?;
    }
    Ok(rotated)
}
