//! Operator maintenance: delete, reconcile, and signing-key rotation.
//!
//! Separate from [`crate::ingest::Ingest`] because the inputs are genuinely different. A
//! delete needs no signing key and no compression level, and folding it into the serving
//! object would mean every caller had to invent values it never uses.
//!
//! Nothing here runs on the serving path.

use snafu::ResultExt as _;

/// Records re-signed per `redb` transaction during a rotation. Large enough that the fsync
/// per commit is amortized, small enough that the pending writes of one batch are a bounded
/// amount of memory.
const BATCH: usize = 1000;

#[derive(Debug, snafu::Snafu)]
#[snafu(visibility(pub))]
pub enum Error {
    #[snafu(display("the index rejected the operation"))]
    Index { source: bincache_index::index::Error },

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

/// Forget the record. The artifact stays.
///
/// Leaving it is deliberate. A NAR does not include the store path name, so two paths whose
/// contents are identical produce the same `NarHash`, the same `FileHash`, and therefore
/// one shared artifact on disk. Unlinking it here would strand the other path's narinfo
/// pointing at bytes that are gone, which is the loudest failure this cache has: a client
/// that resolved the metadata then asks for a NAR that is not there.
///
/// Deciding whether anything still references an artifact takes a pass over every record,
/// which is what [`reconcile`] is for. Leaving it also makes a delete O(1).
pub fn delete(
    index: &bincache_index::index::Index,
    key: &bincache_core::storepath::Hash,
) -> Result<Deleted, Error> {
    let Some(record) = index.unpublish(key).context(IndexSnafu)? else {
        return Ok(Deleted::Absent);
    };
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
///
/// Committed in batches. `redb` fsyncs per commit, so re-signing through the one-record
/// publish used by a push meant one fsync per path: a million-path cache spent a million
/// fsyncs on the procedure the README documents for key rotation.
pub fn rotate(
    index: &bincache_index::index::Index,
    dir: &bincache_core::storepath::Dir,
    key: &bincache_core::sign::SecretKey,
) -> Result<usize, Error> {
    let mut records = index.records().context(IndexSnafu)?;
    let rotated = records.len();
    for record in &mut records {
        record.resign(dir, key);
    }
    for batch in records.chunks(BATCH) {
        index.republish(batch, dir).context(IndexSnafu)?;
        tracing::info!(rotated = batch.len(), total = rotated, "re-signing");
    }
    Ok(rotated)
}
