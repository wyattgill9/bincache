//! The `redb` schema and the operations the rest of the system performs against it.
//!
//! Two tables, one writer discipline. A commit is the publish: there is no snapshot to
//! validate at boot, no log to replay, and no O(n) filesystem rescan as the corruption
//! floor, because the store owns crash consistency.
//!
//! `fjall` is the named alternative if write throughput ever dominates, since an LSM
//! absorbs bursts better than a B-tree. That would be a measured swap behind this API.

use redb::ReadableDatabase as _;
use redb::ReadableTable as _;
use redb::TableHandle as _;
use snafu::ResultExt as _;

/// `StorePathHash` to the signed narinfo record. The primary key of the whole protocol.
const NARINFO: redb::TableDefinition<'static, &[u8; bincache_core::storepath::HASH_WIDTH], &[u8]> =
    redb::TableDefinition::new("narinfo");

/// `NarHash` to the artifact that holds those NAR bytes.
const NAR: redb::TableDefinition<'static, &[u8; bincache_core::hash::WIDTH], &[u8]> =
    redb::TableDefinition::new("nar");

#[derive(Debug, snafu::Snafu)]
#[snafu(visibility(pub))]
pub enum Error {
    #[snafu(display("opening the index at {} failed", path.display()))]
    Open { path: std::path::PathBuf, source: redb::DatabaseError },

    #[snafu(display("beginning a read transaction failed"))]
    BeginRead { source: redb::TransactionError },

    #[snafu(display("beginning a write transaction failed"))]
    BeginWrite { source: redb::TransactionError },

    #[snafu(display("opening table {table} failed"))]
    Table { table: &'static str, source: redb::TableError },

    #[snafu(display("reading from the index failed"))]
    Read { source: redb::StorageError },

    #[snafu(display("writing to the index failed"))]
    Write { source: redb::StorageError },

    #[snafu(display("committing the publish failed"))]
    Commit { source: redb::CommitError },

    #[snafu(display("encoding a record failed"))]
    Encode { source: rkyv::rancor::Error },

    #[snafu(display("a stored record does not decode; the index is corrupt"))]
    Decode { source: rkyv::rancor::Error },
}

/// Cheap to clone: shards share one instance built at the composition root.
#[derive(Clone)]
pub struct Index {
    database: std::sync::Arc<redb::Database>,
}

impl Index {
    /// Opens, creating the file if it is absent. Both tables are created here so a read on
    /// a fresh database answers "absent" rather than "no such table".
    pub fn open(path: std::path::PathBuf) -> Result<Self, Error> {
        let database = redb::Database::create(&path).context(OpenSnafu { path: path.clone() })?;
        let index = Self { database: std::sync::Arc::new(database) };

        let transaction = index.database.begin_write().context(BeginWriteSnafu)?;
        transaction.open_table(NARINFO).context(TableSnafu { table: NARINFO.name() })?;
        transaction.open_table(NAR).context(TableSnafu { table: NAR.name() })?;
        transaction.commit().context(CommitSnafu)?;
        Ok(index)
    }

    /// The record for a metadata request. `None` is the common answer: closure resolution
    /// probes for paths the cache may not hold.
    pub fn narinfo(
        &self,
        key: &bincache_core::storepath::Hash,
    ) -> Result<Option<bincache_core::narinfo::NarInfo>, Error> {
        let transaction = self.database.begin_read().context(BeginReadSnafu)?;
        let table =
            transaction.open_table(NARINFO).context(TableSnafu { table: NARINFO.name() })?;
        let Some(stored) = table.get(key.as_bytes()).context(ReadSnafu)? else {
            return Ok(None);
        };
        let record = decode_narinfo(stored.value())?;
        Ok(Some(record))
    }

    /// Where the NAR with this hash was stored, if it was.
    pub fn nar(
        &self,
        nar_hash: &bincache_core::hash::Sha256,
    ) -> Result<Option<crate::nar::Entry>, Error> {
        let transaction = self.database.begin_read().context(BeginReadSnafu)?;
        let table = transaction.open_table(NAR).context(TableSnafu { table: NAR.name() })?;
        let Some(stored) = table.get(nar_hash.as_bytes()).context(ReadSnafu)? else {
            return Ok(None);
        };
        let entry = decode_nar(stored.value())?;
        Ok(Some(entry))
    }

    /// Records a committed artifact. Runs after the NAR file is durable, so a crash between
    /// the two leaves an orphan file that reconciliation collects, never a record with no
    /// bytes behind it.
    pub fn put_nar(
        &self,
        nar_hash: &bincache_core::hash::Sha256,
        entry: &crate::nar::Entry,
    ) -> Result<(), Error> {
        let encoded = rkyv::to_bytes(entry).context(EncodeSnafu)?;
        let transaction = self.database.begin_write().context(BeginWriteSnafu)?;
        {
            let mut table =
                transaction.open_table(NAR).context(TableSnafu { table: NAR.name() })?;
            table.insert(nar_hash.as_bytes(), encoded.as_ref()).context(WriteSnafu)?;
        }
        transaction.commit().context(CommitSnafu)
    }

    /// The publish. Everything expensive (compression, rendering, signing) already ran; this
    /// commit is the single point where a path becomes visible to readers.
    pub fn publish(&self, record: &bincache_core::narinfo::NarInfo) -> Result<(), Error> {
        let encoded = rkyv::to_bytes(record).context(EncodeSnafu)?;
        let key = record.store_path.hash().as_bytes();

        let transaction = self.database.begin_write().context(BeginWriteSnafu)?;
        {
            let mut table =
                transaction.open_table(NARINFO).context(TableSnafu { table: NARINFO.name() })?;
            table.insert(key, encoded.as_ref()).context(WriteSnafu)?;
        }
        transaction.commit().context(CommitSnafu)
    }

    /// Operator-triggered delete. Returns the record so the caller can unlink the artifact
    /// it named; the index performs no filesystem work of its own.
    pub fn unpublish(
        &self,
        key: &bincache_core::storepath::Hash,
    ) -> Result<Option<bincache_core::narinfo::NarInfo>, Error> {
        let transaction = self.database.begin_write().context(BeginWriteSnafu)?;
        let removed = {
            let mut table =
                transaction.open_table(NARINFO).context(TableSnafu { table: NARINFO.name() })?;
            let mut nar = transaction.open_table(NAR).context(TableSnafu { table: NAR.name() })?;
            match table.remove(key.as_bytes()).context(WriteSnafu)? {
                None => None,
                Some(stored) => {
                    let record = decode_narinfo(stored.value())?;
                    drop(stored);
                    nar.remove(record.nar_hash.as_bytes()).context(WriteSnafu)?;
                    Some(record)
                }
            }
        };
        transaction.commit().context(CommitSnafu)?;
        Ok(removed)
    }

    /// Every record, for reconciliation against the filesystem and for a signing-key
    /// rotation pass. Materialized rather than streamed so the read transaction, and the
    /// snapshot it pins, ends before the caller starts writing.
    pub fn records(&self) -> Result<Vec<bincache_core::narinfo::NarInfo>, Error> {
        let transaction = self.database.begin_read().context(BeginReadSnafu)?;
        let table =
            transaction.open_table(NARINFO).context(TableSnafu { table: NARINFO.name() })?;
        let mut records = Vec::new();
        for row in table.iter().context(ReadSnafu)? {
            let (_key, stored) = row.context(ReadSnafu)?;
            records.push(decode_narinfo(stored.value())?);
        }
        Ok(records)
    }

    pub fn count(&self) -> Result<u64, Error> {
        let transaction = self.database.begin_read().context(BeginReadSnafu)?;
        let table =
            transaction.open_table(NARINFO).context(TableSnafu { table: NARINFO.name() })?;
        redb::ReadableTableMetadata::len(&table).context(ReadSnafu)
    }
}

fn decode_narinfo(bytes: &[u8]) -> Result<bincache_core::narinfo::NarInfo, Error> {
    let archived: &bincache_core::narinfo::ArchivedNarInfo =
        rkyv::access(bytes).context(DecodeSnafu)?;
    let record: bincache_core::narinfo::NarInfo =
        rkyv::deserialize(archived).context(DecodeSnafu)?;
    Ok(record)
}

fn decode_nar(bytes: &[u8]) -> Result<crate::nar::Entry, Error> {
    let archived: &crate::nar::ArchivedEntry = rkyv::access(bytes).context(DecodeSnafu)?;
    let entry: crate::nar::Entry = rkyv::deserialize(archived).context(DecodeSnafu)?;
    Ok(entry)
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    fn path(name: &str) -> std::path::PathBuf {
        let directory =
            std::path::PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../../target"))
                .join("test-artifacts")
                .join("bincache-index");
        std::fs::create_dir_all(&directory).expect("creates the artifact directory");
        let path = directory.join(format!("{name}.redb"));
        if let Err(error) = std::fs::remove_file(&path) {
            assert_eq!(error.kind(), std::io::ErrorKind::NotFound, "stale index not removable");
        }
        path
    }

    fn dir() -> bincache_core::storepath::Dir {
        bincache_core::storepath::Dir::new(bincache_core::storepath::DIR_DEFAULT.to_owned())
            .expect("absolute")
    }

    fn record(name: &str) -> bincache_core::narinfo::NarInfo {
        let hash = bincache_core::hash::Sha256::digest(name.as_bytes());
        let mut key = [0u8; bincache_core::storepath::HASH_WIDTH];
        key.copy_from_slice(&hash.as_bytes()[..bincache_core::storepath::HASH_WIDTH]);

        let mut record = bincache_core::narinfo::NarInfo {
            store_path: bincache_core::storepath::Path::new(
                bincache_core::storepath::Hash::from_bytes(key),
                bincache_core::storepath::Name::new(name.to_owned()).expect("legal"),
            ),
            compression: bincache_core::compression::Compression::Zstd,
            file_hash: bincache_core::hash::Sha256::digest(b"artifact"),
            file_size: 512,
            nar_hash: hash,
            nar_size: core::num::NonZeroU64::new(1024).expect("nonzero"),
            references: Vec::new(),
            deriver: None,
            sigs: Vec::new(),
            ca: None,
        };
        let key = bincache_core::sign::SecretKey::generate("bincache-test-1".to_owned());
        record.resign(&dir(), &key);
        record
    }

    #[test]
    fn publishes_and_reads_back_an_exact_record() {
        let index = crate::index::Index::open(path("publish")).expect("opens");
        let published = record("hello");
        index.publish(&published).expect("publishes");

        let read = index.narinfo(published.store_path.hash()).expect("reads").expect("present");
        assert_eq!(read, published);
        assert_eq!(read.render(&dir()), published.render(&dir()));
    }

    #[test]
    fn reports_an_absent_key_rather_than_failing() {
        let index = crate::index::Index::open(path("absent")).expect("opens");
        let missing = bincache_core::storepath::Hash::from_bytes([0u8; 20]);
        assert!(index.narinfo(&missing).expect("reads").is_none());
        assert_eq!(index.count().expect("counts"), 0);
    }

    #[test]
    fn republishing_the_same_path_is_idempotent() {
        let index = crate::index::Index::open(path("idempotent")).expect("opens");
        let published = record("hello");
        index.publish(&published).expect("publishes");
        index.publish(&published).expect("republishes");
        assert_eq!(index.count().expect("counts"), 1);
    }

    #[test]
    fn unpublish_returns_the_record_and_forgets_its_nar() {
        let index = crate::index::Index::open(path("unpublish")).expect("opens");
        let published = record("hello");
        let entry = crate::nar::Entry {
            file_hash: published.file_hash,
            file_size: published.file_size,
            nar_size: published.nar_size,
            compression: published.compression,
        };
        index.put_nar(&published.nar_hash, &entry).expect("stages");
        index.publish(&published).expect("publishes");

        let removed =
            index.unpublish(published.store_path.hash()).expect("unpublishes").expect("present");
        assert_eq!(removed, published);
        assert!(index.narinfo(published.store_path.hash()).expect("reads").is_none());
        assert!(index.nar(&published.nar_hash).expect("reads").is_none());
        assert!(index.unpublish(published.store_path.hash()).expect("unpublishes").is_none());
    }

    #[test]
    fn nar_entries_survive_a_round_trip() {
        let index = crate::index::Index::open(path("nar")).expect("opens");
        let nar_hash = bincache_core::hash::Sha256::digest(b"uncompressed");
        let entry = crate::nar::Entry {
            file_hash: bincache_core::hash::Sha256::digest(b"compressed"),
            file_size: 99,
            nar_size: core::num::NonZeroU64::new(1234).expect("nonzero"),
            compression: bincache_core::compression::Compression::Zstd,
        };
        index.put_nar(&nar_hash, &entry).expect("stages");
        assert_eq!(index.nar(&nar_hash).expect("reads"), Some(entry));
        assert_eq!(entry.url().name(), format!("{}.nar.zst", entry.file_hash.base32()));
    }

    #[test]
    fn records_lists_everything_published() {
        let index = crate::index::Index::open(path("records")).expect("opens");
        for name in ["one", "two", "three"] {
            index.publish(&record(name)).expect("publishes");
        }
        assert_eq!(index.records().expect("lists").len(), 3);
        assert_eq!(index.count().expect("counts"), 3);
    }

    /// Reopening must see what a previous process committed: the commit is the publish, and
    /// there is nothing else to replay.
    #[test]
    fn a_reopened_index_sees_what_was_committed() {
        let path = path("reopen");
        let published = record("hello");
        {
            let index = crate::index::Index::open(path.clone()).expect("opens");
            index.publish(&published).expect("publishes");
        }
        let index = crate::index::Index::open(path).expect("reopens");
        assert_eq!(index.narinfo(published.store_path.hash()).expect("reads"), Some(published));
    }
}
