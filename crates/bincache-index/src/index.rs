//! The `lkv` schema and the operations the rest of the system performs against it.
//!
//! Two logical tables, one writer discipline. A commit is the publish: there is no snapshot
//! to validate at boot, no log to replay, and no O(n) filesystem rescan as the corruption
//! floor, because the store owns crash consistency.
//!
//! `lkv` keeps one flat keyspace and answers a lookup from a hash table over an mmap, which
//! is the shape this index wants: the read path is a point lookup by store path hash, and
//! ordered or ranged queries are never asked for. Writes append to an overlay that
//! [`compact`](lkv::Database::compact) folds back into the base, so publish throughput is
//! bounded by that fold rather than by a B-tree rebalance.

use snafu::ResultExt as _;

/// `lkv` has one flat keyspace, so a leading tag byte says which logical table a key
/// belongs to. The two key widths differ as well, but a tag states the intent rather than
/// leaving it to be inferred from a length that could later collide.
const NARINFO: u8 = b'p';

/// `NarHash` to the artifact that holds those NAR bytes.
const NAR: u8 = b'n';

/// Refuse to grow the file past this. Well above any cache that fits on one machine, and
/// present so a runaway write fails loudly rather than filling the disk.
const DATABASE_BYTES_MAX: u64 = 1 << 40;

/// How much unfolded overlay `lkv` may hold before a write has to compact first. The fold
/// is O(records), so this trades a periodic stall against the memory the overlay pins.
#[cfg(not(test))]
const OVERLAY_MEMORY_BYTES_MAX: usize = 64 * 1024 * 1024;

/// Small enough that a test trips the fold in a few hundred publishes. Only the threshold
/// changes; the fold and the write that follows it are the shipped ones.
#[cfg(test)]
const OVERLAY_MEMORY_BYTES_MAX: usize = 16 * 1024;

#[derive(Debug, snafu::Snafu)]
#[snafu(visibility(pub))]
pub enum Error {
    #[snafu(display("opening the index at {} failed", path.display()))]
    Open { path: std::path::PathBuf, source: lkv::Error },

    #[snafu(display("beginning a write transaction failed"))]
    BeginWrite { source: lkv::Error },

    #[snafu(display("folding the overlay back into the base failed"))]
    Compact { source: lkv::Error },

    #[snafu(display("reading from the index failed"))]
    Read { source: lkv::Error },

    #[snafu(display("writing to the index failed"))]
    Write { source: lkv::Error },

    #[snafu(display("committing the publish failed"))]
    Commit { source: lkv::Error },

    #[snafu(display("encoding a record failed"))]
    Encode { source: rkyv::rancor::Error },

    #[snafu(display("a stored record does not decode; the index is corrupt"))]
    Decode { source: rkyv::rancor::Error },
}

/// Cheap to clone: shards share one instance built at the composition root.
///
/// `lkv` takes `&mut` for a write and `&` for a read, so the lock is what the store already
/// asks for in the type system. Readers on the serving path run concurrently; the single
/// writer excludes them for the length of one commit.
#[derive(Clone)]
pub struct Index {
    database: std::sync::Arc<std::sync::RwLock<lkv::Database>>,
}

impl Index {
    /// Opens, creating the file if it is absent.
    pub fn open(path: std::path::PathBuf) -> Result<Self, Error> {
        let options = lkv::DatabaseOptions::default()
            .with_verification(lkv::VerificationMode::OnRead)
            .with_max_database_bytes(DATABASE_BYTES_MAX)
            .with_overlay_memory_limit(OVERLAY_MEMORY_BYTES_MAX);

        // `lkv::Database::create` refuses an existing file, so absence is what selects it.
        let opened = match lkv::Database::open_with_options(&path, options.clone()) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                lkv::Database::create_with_options(&path, options)
            }
            opened => opened,
        };
        let database = opened.context(OpenSnafu { path })?;
        Ok(Self { database: std::sync::Arc::new(std::sync::RwLock::new(database)) })
    }

    /// The record for a metadata request. `None` is the common answer: closure resolution
    /// probes for paths the cache may not hold.
    pub fn narinfo(
        &self,
        key: &bincache_core::storepath::Hash,
    ) -> Result<Option<bincache_core::narinfo::NarInfo>, Error> {
        let database = self.read();
        let Some(stored) = database.get(narinfo_key(key)).context(ReadSnafu)? else {
            return Ok(None);
        };
        let record = decode_narinfo(stored)?;
        Ok(Some(record))
    }

    /// Where the NAR with this hash was stored, if it was.
    pub fn nar(
        &self,
        nar_hash: &bincache_core::hash::Sha256,
    ) -> Result<Option<crate::nar::Entry>, Error> {
        let database = self.read();
        let Some(stored) = database.get(nar_key(nar_hash)).context(ReadSnafu)? else {
            return Ok(None);
        };
        let entry = decode_nar(stored)?;
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
        let mut database = self.write();
        let mut transaction = begin_write(&mut database)?;
        transaction.put(nar_key(nar_hash), encoded.as_ref()).context(WriteSnafu)?;
        transaction.commit().context(CommitSnafu)
    }

    /// The publish. Everything expensive (compression, rendering, signing) already ran; this
    /// commit is the single point where a path becomes visible to readers.
    pub fn publish(&self, record: &bincache_core::narinfo::NarInfo) -> Result<(), Error> {
        let encoded = rkyv::to_bytes(record).context(EncodeSnafu)?;
        let key = narinfo_key(record.store_path.hash());

        let mut database = self.write();
        let mut transaction = begin_write(&mut database)?;
        transaction.put(key, encoded.as_ref()).context(WriteSnafu)?;
        transaction.commit().context(CommitSnafu)
    }

    /// Operator-triggered delete. Returns the record so the caller can unlink the artifact
    /// it named; the index performs no filesystem work of its own.
    pub fn unpublish(
        &self,
        key: &bincache_core::storepath::Hash,
    ) -> Result<Option<bincache_core::narinfo::NarInfo>, Error> {
        let mut database = self.write();
        let mut transaction = begin_write(&mut database)?;
        let Some(stored) = transaction.get(narinfo_key(key)).context(ReadSnafu)? else {
            return Ok(None);
        };
        let record = decode_narinfo(stored)?;
        transaction.delete(narinfo_key(key)).context(WriteSnafu)?;
        transaction.delete(nar_key(&record.nar_hash)).context(WriteSnafu)?;
        transaction.commit().context(CommitSnafu)?;
        Ok(Some(record))
    }

    /// Every record, for reconciliation against the filesystem and for a signing-key
    /// rotation pass. Materialized rather than streamed so the read lock, and the writers it
    /// excludes, is released before the caller starts writing.
    pub fn records(&self) -> Result<Vec<bincache_core::narinfo::NarInfo>, Error> {
        let database = self.read();
        let mut records = Vec::new();
        for row in database.iter().context(ReadSnafu)? {
            let (key, stored) = row.context(ReadSnafu)?;
            let Some(&NARINFO) = key.first() else {
                continue;
            };
            records.push(decode_narinfo(stored)?);
        }
        Ok(records)
    }

    /// How many paths the cache serves. A full unordered scan, because `lkv` counts by
    /// walking and this count is over one tag rather than the whole keyspace. Cheap enough
    /// for a metrics scrape, not for a request path.
    pub fn count(&self) -> Result<u64, Error> {
        let database = self.read();
        let mut count: u64 = 0;
        for row in database.iter().context(ReadSnafu)? {
            let (key, _stored) = row.context(ReadSnafu)?;
            let Some(&NARINFO) = key.first() else {
                continue;
            };
            count += 1;
        }
        Ok(count)
    }

    /// A panic while the lock was held cannot leave the database half-written: `lkv` tracks
    /// its own handle health and answers [`lkv::Error::Poisoned`] on the next call, so the
    /// lock's poison flag carries nothing this layer would act on.
    fn read(&self) -> std::sync::RwLockReadGuard<'_, lkv::Database> {
        self.database.read().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn write(&self) -> std::sync::RwLockWriteGuard<'_, lkv::Database> {
        self.database.write().unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// `lkv` refuses a write once the overlay outgrows its limit and expects the caller to
/// compact. Folding here keeps that recovery bounded (one fold, then the write) and owned
/// by the one type that holds the writer.
fn begin_write(database: &mut lkv::Database) -> Result<lkv::WriteTransaction<'_>, Error> {
    let stats = database.stats().context(ReadSnafu)?;
    if stats.overlay_memory_bytes > OVERLAY_MEMORY_BYTES_MAX {
        tracing::warn!(
            overlay_memory_bytes = stats.overlay_memory_bytes,
            limit = OVERLAY_MEMORY_BYTES_MAX,
            "folding the index overlay into its base"
        );
        database.compact().context(CompactSnafu)?;
    } else {
        // Room left in the overlay; the write appends.
    }
    database.begin_write().context(BeginWriteSnafu)
}

/// `StorePathHash` to the signed narinfo record. The primary key of the whole protocol.
fn narinfo_key(
    key: &bincache_core::storepath::Hash,
) -> [u8; 1 + bincache_core::storepath::HASH_WIDTH] {
    let mut tagged = [NARINFO; 1 + bincache_core::storepath::HASH_WIDTH];
    tagged[1..].copy_from_slice(key.as_bytes());
    tagged
}

fn nar_key(nar_hash: &bincache_core::hash::Sha256) -> [u8; 1 + bincache_core::hash::WIDTH] {
    let mut tagged = [NAR; 1 + bincache_core::hash::WIDTH];
    tagged[1..].copy_from_slice(nar_hash.as_bytes());
    tagged
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
        let path = directory.join(format!("{name}.lkv"));
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

    /// Both logical tables share one flat keyspace, so a scan that forgot its tag would
    /// count the staged NAR entry as a fourth path and fail to decode it as a record.
    #[test]
    fn records_lists_everything_published_and_nothing_else() {
        let index = crate::index::Index::open(path("records")).expect("opens");
        for name in ["one", "two", "three"] {
            index.publish(&record(name)).expect("publishes");
        }
        let staged = record("four");
        index
            .put_nar(
                &staged.nar_hash,
                &crate::nar::Entry {
                    file_hash: staged.file_hash,
                    file_size: staged.file_size,
                    nar_size: staged.nar_size,
                    compression: staged.compression,
                },
            )
            .expect("stages");

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

    /// `lkv` refuses a write once the overlay outgrows its limit, so publishing past that
    /// limit is the failure this index has to absorb rather than surface. Enough writes to
    /// trip the fold more than once, then every record still reads back.
    #[test]
    fn publishing_past_the_overlay_limit_folds_and_keeps_going() {
        let index = crate::index::Index::open(path("overlay")).expect("opens");
        let published: Vec<bincache_core::narinfo::NarInfo> =
            (0..256u32).map(|serial| record(&format!("overlay-{serial}"))).collect();
        for record in &published {
            index.publish(record).expect("publishes");
        }

        assert_eq!(index.count().expect("counts"), 256);
        for record in &published {
            assert_eq!(
                index.narinfo(record.store_path.hash()).expect("reads").as_ref(),
                Some(record)
            );
        }
    }
}
