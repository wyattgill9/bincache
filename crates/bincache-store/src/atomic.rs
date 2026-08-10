//! A directory of small immutable files, each appearing whole or not at all.
//!
//! Both things bincache keeps besides the artifacts themselves (a rendered narinfo body and
//! a NAR receipt) are the same shape: a few hundred bytes, named by a hash, written once,
//! never edited. The write discipline that makes one of them durable is the discipline that
//! makes the other durable, and a second copy of it would be a second place to get a power
//! loss wrong.
//!
//! Write, `fsync`, rename, `fsync` the directory. The rename is the publish: a reader sees
//! either the previous contents or the new ones, never a partial file, and the directory
//! sync is what keeps the new name after the machine loses power rather than only after the
//! process dies.

use snafu::ResultExt as _;
use tokio::io::AsyncWriteExt as _;

/// Prefix for a file being written. Leading dot so [`list`] can tell a half-written file
/// from a published one by name alone, without stat-ing anything.
const TEMPORARY: &str = ".tmp-";

#[derive(Debug, snafu::Snafu)]
#[snafu(visibility(pub))]
pub enum Error {
    #[snafu(display("creating directory {} failed", path.display()))]
    CreateDir { path: std::path::PathBuf, source: std::io::Error },

    #[snafu(display("creating {} failed", path.display()))]
    Create { path: std::path::PathBuf, source: std::io::Error },

    #[snafu(display("writing {} failed", path.display()))]
    Write { path: std::path::PathBuf, source: std::io::Error },

    #[snafu(display("flushing {} to disk failed", path.display()))]
    Sync { path: std::path::PathBuf, source: std::io::Error },

    #[snafu(display("linking {} into place at {} failed", from.display(), to.display()))]
    Rename { from: std::path::PathBuf, to: std::path::PathBuf, source: std::io::Error },

    #[snafu(display("reading {} failed", path.display()))]
    Read { path: std::path::PathBuf, source: std::io::Error },

    #[snafu(display("unlinking {} failed", path.display()))]
    Remove { path: std::path::PathBuf, source: std::io::Error },

    #[snafu(display("listing {} failed", path.display()))]
    List { path: std::path::PathBuf, source: std::io::Error },
}

/// Whether a write created a name that was not there before.
///
/// Republishing a path is idempotent and routine, so this is not a success signal. It is
/// what lets a caller keep a count of distinct paths without walking the directory.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Wrote {
    Created,
    Replaced,
}

/// Whether an unlink found anything.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Removed {
    Deleted,
    Absent,
}

/// One directory of atomically-written files, plus the counter that keeps concurrent
/// temporary names in this process distinct.
#[derive(Clone, Debug)]
pub struct Dir {
    path: std::path::PathBuf,
    sequence: std::sync::Arc<core::sync::atomic::AtomicU64>,
}

impl Dir {
    /// Creates the directory if it is absent. Idempotent, so this is also the boot path for
    /// an existing one.
    pub async fn open(path: std::path::PathBuf) -> Result<Self, Error> {
        tokio::fs::create_dir_all(&path).await.context(CreateDirSnafu { path: path.clone() })?;
        let sequence = std::sync::Arc::new(core::sync::atomic::AtomicU64::new(0));
        Ok(Self { path, sequence })
    }

    #[must_use]
    pub fn path(&self, name: &str) -> std::path::PathBuf {
        self.path.join(name)
    }

    /// `None` means absent, which is the routine answer on a cache and not a fault.
    ///
    /// Reads synchronously, on the caller's task, on purpose. `tokio::fs` is not
    /// asynchronous file I/O: it hands every operation to a blocking threadpool, so a
    /// read that hits the page cache pays a thread handoff far larger than the read. On
    /// the metadata path that measured as a 48% throughput loss and a p99 of 10ms against
    /// 432us, all of it queueing for the pool.
    ///
    /// These files are a few hundred bytes and are the hottest thing the cache serves, so
    /// the page cache answers essentially all of them in microseconds. A cold read does
    /// block a worker, which is the same exposure the previous embedded database had when
    /// it faulted in a B-tree page, and is why the payload plane (unbounded, frequently
    /// cold) still streams through the pool.
    pub fn read(&self, name: &str) -> Result<Option<Vec<u8>>, Error> {
        let path = self.path(name);
        let read = std::fs::read(&path);
        if absent(&read) {
            return Ok(None);
        }
        read.context(ReadSnafu { path }).map(Some)
    }

    /// Write, `fsync`, rename, `fsync` the directory. See the module doc for why each step
    /// is there.
    ///
    /// A concurrent write of the same name races here and wins harmlessly: everything
    /// bincache stores under a hash is a function of that hash, so both writers produced
    /// the same bytes.
    pub async fn write(&self, name: &str, contents: &[u8]) -> Result<Wrote, Error> {
        let ordinal = self.sequence.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        let temporary = self.path(&format!("{TEMPORARY}{}-{ordinal}", std::process::id()));
        let target = self.path(name);

        let mut file = tokio::fs::File::create(&temporary)
            .await
            .context(CreateSnafu { path: temporary.clone() })?;
        file.write_all(contents).await.context(WriteSnafu { path: temporary.clone() })?;
        file.sync_all().await.context(SyncSnafu { path: temporary.clone() })?;
        drop(file);

        // Checked before the rename rather than after, because the rename is what makes the
        // answer stop being true.
        let existed =
            tokio::fs::try_exists(&target).await.context(ReadSnafu { path: target.clone() })?;

        tokio::fs::rename(&temporary, &target)
            .await
            .context(RenameSnafu { from: temporary, to: target })?;
        self.sync().await?;

        Ok(if existed { Wrote::Replaced } else { Wrote::Created })
    }

    pub async fn remove(&self, name: &str) -> Result<Removed, Error> {
        let path = self.path(name);
        let removed = tokio::fs::remove_file(&path).await;
        if absent(&removed) {
            return Ok(Removed::Absent);
        }
        removed.context(RemoveSnafu { path })?;
        self.sync().await?;
        Ok(Removed::Deleted)
    }

    /// Every published name. Files still being written are skipped: they carry the
    /// [`TEMPORARY`] prefix and are not yet anything.
    ///
    /// Blocking on purpose. This runs at boot and during maintenance, never on the serving
    /// path, and a blocking walk keeps the ordering obvious.
    pub fn list(&self) -> Result<Vec<String>, Error> {
        let listing = std::fs::read_dir(&self.path).context(ListSnafu { path: &self.path })?;
        let mut names = Vec::new();
        for entry in listing {
            let entry = entry.context(ListSnafu { path: &self.path })?;
            let name = entry.file_name().to_str().map(str::to_owned);
            match name {
                Some(name) if name.starts_with(TEMPORARY) => {}
                Some(name) => names.push(name),
                // A name that is not UTF-8 was not written by bincache: every name it
                // writes is base32.
                None => {}
            }
        }
        Ok(names)
    }

    /// Deletes temporary files a crash left behind. Safe at any time: a live write holds
    /// its descriptor, so unlinking only means the crashed writer's bytes go nowhere.
    pub async fn sweep(&self) -> Result<usize, Error> {
        let listing = std::fs::read_dir(&self.path).context(ListSnafu { path: &self.path })?;
        let mut swept = 0;
        for entry in listing {
            let entry = entry.context(ListSnafu { path: &self.path })?;
            let temporary =
                entry.file_name().to_str().is_some_and(|name| name.starts_with(TEMPORARY));
            if temporary {
                let path = entry.path();
                tokio::fs::remove_file(&path).await.context(RemoveSnafu { path })?;
                swept += 1;
            }
        }
        Ok(swept)
    }

    async fn sync(&self) -> Result<(), Error> {
        let directory =
            tokio::fs::File::open(&self.path).await.context(ReadSnafu { path: &self.path })?;
        directory.sync_all().await.context(SyncSnafu { path: &self.path })
    }
}

/// `true` when the operation failed only because the target is not there.
fn absent<T>(result: &Result<T, std::io::Error>) -> bool {
    matches!(result, Err(source) if source.kind() == std::io::ErrorKind::NotFound)
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    async fn dir(name: &str) -> crate::atomic::Dir {
        let path = std::path::PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../../target"))
            .join("test-artifacts")
            .join("bincache-atomic")
            .join(name);
        if let Err(error) = std::fs::remove_dir_all(&path) {
            assert_eq!(error.kind(), std::io::ErrorKind::NotFound, "stale dir not removable");
        }
        crate::atomic::Dir::open(path).await.expect("opens")
    }

    #[tokio::test]
    async fn writes_and_reads_back_exact_bytes() {
        let dir = dir("roundtrip").await;
        assert_eq!(
            dir.write("key", b"contents").await.expect("writes"),
            crate::atomic::Wrote::Created
        );
        assert_eq!(dir.read("key").expect("reads"), Some(b"contents".to_vec()));
    }

    #[tokio::test]
    async fn reports_an_absent_name_rather_than_failing() {
        let dir = dir("absent").await;
        assert_eq!(dir.read("missing").expect("reads"), None);
        assert_eq!(dir.remove("missing").await.expect("removes"), crate::atomic::Removed::Absent);
    }

    /// Republishing is routine, and the caller counting distinct paths has to be able to
    /// tell it apart from a first publish.
    #[tokio::test]
    async fn distinguishes_a_first_write_from_a_replacement() {
        let dir = dir("replace").await;
        assert_eq!(
            dir.write("key", b"first").await.expect("writes"),
            crate::atomic::Wrote::Created
        );
        assert_eq!(
            dir.write("key", b"second").await.expect("rewrites"),
            crate::atomic::Wrote::Replaced
        );
        assert_eq!(dir.read("key").expect("reads"), Some(b"second".to_vec()));
        assert_eq!(dir.list().expect("lists"), vec!["key".to_owned()]);
    }

    #[tokio::test]
    async fn lists_and_removes_published_names() {
        let dir = dir("list").await;
        for name in ["one", "two", "three"] {
            dir.write(name, b"x").await.expect("writes");
        }
        let mut listed = dir.list().expect("lists");
        listed.sort();
        assert_eq!(listed, vec!["one".to_owned(), "three".to_owned(), "two".to_owned()]);

        assert_eq!(dir.remove("two").await.expect("removes"), crate::atomic::Removed::Deleted);
        assert_eq!(dir.list().expect("lists").len(), 2);
    }

    /// A temporary file is not a published name, and must never be listed as one: a
    /// reconciliation pass that saw it would report a path the cache does not hold.
    #[tokio::test]
    async fn a_half_written_file_is_neither_listed_nor_kept() {
        let dir = dir("sweep").await;
        dir.write("real", b"x").await.expect("writes");
        std::fs::write(dir.path(".tmp-9999-0"), b"crashed").expect("plants a temporary file");

        assert_eq!(dir.list().expect("lists"), vec!["real".to_owned()]);
        assert_eq!(dir.sweep().await.expect("sweeps"), 1);
        assert_eq!(dir.sweep().await.expect("sweeps"), 0);
        assert_eq!(dir.list().expect("lists"), vec!["real".to_owned()]);
    }
}
