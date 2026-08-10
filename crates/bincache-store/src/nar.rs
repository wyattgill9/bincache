//! Content-addressed NAR files: where they live, how they appear atomically, how they are
//! read back, and how orphans are found.

use snafu::ResultExt as _;
use tokio::io::AsyncWriteExt as _;

/// Holds every committed artifact, under the store root.
const ARTIFACTS: &str = "nar";

/// Holds uploads that have not been committed. A crash leaves files here rather than under
/// [`ARTIFACTS`], so a partial upload is never mistaken for an artifact.
const STAGING: &str = "staging";

/// Leading characters of the file hash used as a shard directory, keeping any one directory
/// well under the sizes where `readdir` and dentry caching start to hurt. The served URL
/// stays flat; this split is private to the filesystem.
const SHARD_LEN: usize = 2;

#[derive(Debug, snafu::Snafu)]
#[snafu(visibility(pub))]
pub enum Error {
    #[snafu(display("creating directory {} failed", path.display()))]
    CreateDir { path: std::path::PathBuf, source: std::io::Error },

    #[snafu(display("opening {} failed", path.display()))]
    Open { path: std::path::PathBuf, source: std::io::Error },

    #[snafu(display("creating staging file {} failed", path.display()))]
    Create { path: std::path::PathBuf, source: std::io::Error },

    #[snafu(display("reading metadata of {} failed", path.display()))]
    Metadata { path: std::path::PathBuf, source: std::io::Error },

    #[snafu(display("writing {} failed", path.display()))]
    Write { path: std::path::PathBuf, source: std::io::Error },

    #[snafu(display("reading {} failed", path.display()))]
    Read { path: std::path::PathBuf, source: std::io::Error },

    #[snafu(display("seeking {} to {offset} failed", path.display()))]
    Seek { path: std::path::PathBuf, offset: u64, source: std::io::Error },

    #[snafu(display("a length does not fit a u64"))]
    Width { source: core::num::TryFromIntError },

    #[snafu(display("flushing {} to disk failed", path.display()))]
    Sync { path: std::path::PathBuf, source: std::io::Error },

    #[snafu(display("linking {} into place at {} failed", from.display(), to.display()))]
    Rename { from: std::path::PathBuf, to: std::path::PathBuf, source: std::io::Error },

    #[snafu(display("unlinking {} failed", path.display()))]
    Remove { path: std::path::PathBuf, source: std::io::Error },

    #[snafu(display("listing {} failed", path.display()))]
    List { path: std::path::PathBuf, source: std::io::Error },
}

/// Whether an unlink found anything. An enum rather than a bool so a call site cannot read
/// `true` as "succeeded".
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Removed {
    Deleted,
    Absent,
}

/// Cheap to clone: every handler shares one instance built at the composition root.
#[derive(Clone, Debug)]
pub struct Store {
    inner: std::sync::Arc<Inner>,
}

#[derive(Debug)]
struct Inner {
    root: std::path::PathBuf,
    /// Distinguishes concurrent staging files within this process.
    sequence: core::sync::atomic::AtomicU64,
}

impl Store {
    /// Creates the directory skeleton if it is not there. Idempotent, so this is also the
    /// boot path for an existing store.
    pub async fn open(root: std::path::PathBuf) -> Result<Self, Error> {
        for path in [root.clone(), root.join(ARTIFACTS), root.join(STAGING)] {
            tokio::fs::create_dir_all(&path).await.context(CreateDirSnafu { path })?;
        }
        let sequence = core::sync::atomic::AtomicU64::new(0);
        Ok(Self { inner: std::sync::Arc::new(Inner { root, sequence }) })
    }

    /// `<root>/nar/<shard>/<file hash>.nar<ext>`.
    #[must_use]
    pub fn path(&self, url: &bincache_core::narurl::NarUrl) -> std::path::PathBuf {
        let name = url.name();
        let shard = &name[..SHARD_LEN];
        self.inner.root.join(ARTIFACTS).join(shard).join(&name)
    }

    /// `None` means the artifact is absent, which is a routine answer on a cache and not a
    /// fault. Every other failure is typed.
    pub async fn read(&self, url: &bincache_core::narurl::NarUrl) -> Result<Option<Reader>, Error> {
        let path = self.path(url);
        let opened = tokio::fs::File::open(&path).await;
        if absent(&opened) {
            return Ok(None);
        }
        let file = opened.context(OpenSnafu { path: path.clone() })?;
        let metadata = file.metadata().await.context(MetadataSnafu { path: path.clone() })?;
        Ok(Some(Reader { file, path, size: metadata.len() }))
    }

    /// Opens a staging file for an upload. Nothing under `nar/` exists until
    /// [`Staged::commit`] runs, so an abandoned upload cannot be served.
    pub async fn stage(&self) -> Result<Staged, Error> {
        let ordinal = self.inner.sequence.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        let name = format!("{}-{ordinal}.nar", std::process::id());
        let path = self.inner.root.join(STAGING).join(name);
        let file =
            tokio::fs::File::create(&path).await.context(CreateSnafu { path: path.clone() })?;
        Ok(Staged { file, path, written: 0, committed: false })
    }

    pub async fn remove(&self, url: &bincache_core::narurl::NarUrl) -> Result<Removed, Error> {
        let path = self.path(url);
        let removed = tokio::fs::remove_file(&path).await;
        if absent(&removed) {
            return Ok(Removed::Absent);
        }
        removed.context(RemoveSnafu { path })?;
        Ok(Removed::Deleted)
    }

    /// Every artifact on disk, for reconciliation against the published records. Names that do not
    /// parse are reported rather than skipped silently, because an unparseable name under
    /// `nar/` means something other than bincache wrote there.
    pub fn scan(&self) -> Result<Scan, Error> {
        let artifacts = self.inner.root.join(ARTIFACTS);
        let mut scan = Scan::default();
        for shard in entries(&artifacts)? {
            for artifact in entries(&shard)? {
                let name = artifact.file_name().and_then(std::ffi::OsStr::to_str);
                let parsed = name.map(bincache_core::narurl::NarUrl::parse);
                match parsed {
                    Some(Ok(url)) => scan.artifacts.push(url),
                    Some(Err(_)) | None => scan.unrecognized.push(artifact),
                }
            }
        }
        Ok(scan)
    }

    /// Deletes staging files left by a crash. Safe at any time: a live upload holds its
    /// descriptor, so unlinking it only means the crashed writer's bytes go nowhere.
    pub async fn sweep_staging(&self) -> Result<usize, Error> {
        let staging = self.inner.root.join(STAGING);
        let mut swept = 0;
        for path in entries(&staging)? {
            tokio::fs::remove_file(&path).await.context(RemoveSnafu { path })?;
            swept += 1;
        }
        Ok(swept)
    }
}

/// `fsync` on a directory, which is what makes a rename into it survive a power loss.
///
/// Syncing the file only guarantees its contents. The directory entry that gives those
/// contents a name is a separate write, and without this it can still be missing after a
/// crash: the artifact would be durable and invisible.
pub async fn sync_dir(path: &std::path::Path) -> Result<(), Error> {
    let directory = tokio::fs::File::open(path).await.context(OpenSnafu { path })?;
    directory.sync_all().await.context(SyncSnafu { path })
}

/// What a filesystem walk found. Split rather than merged so a caller can act on orphans
/// and on foreign files differently.
#[derive(Debug, Default)]
pub struct Scan {
    pub artifacts: Vec<bincache_core::narurl::NarUrl>,
    pub unrecognized: Vec<std::path::PathBuf>,
}

/// An open artifact plus the size a `Content-Length` needs. The descriptor keeps the file
/// alive across an unlink, so a delete during a stream finishes the stream safely.
#[derive(Debug)]
pub struct Reader {
    file: tokio::fs::File,
    path: std::path::PathBuf,
    size: u64,
}

impl Reader {
    #[must_use]
    pub const fn size(&self) -> u64 {
        self.size
    }

    /// Positions the artifact at `offset` and hands the descriptor over, so the caller can
    /// stream the rest of it straight to a socket.
    ///
    /// Consuming `self` is deliberate: a positioned reader has state a second caller would
    /// not expect, and the size this type carried has already been used to resolve the
    /// range being served.
    pub async fn seek(mut self, offset: u64) -> Result<tokio::fs::File, Error> {
        tokio::io::AsyncSeekExt::seek(&mut self.file, std::io::SeekFrom::Start(offset))
            .await
            .context(SeekSnafu { path: self.path.clone(), offset })?;
        Ok(self.file)
    }
}

/// An upload in flight. Committing is the only way bytes become visible under `nar/`, and
/// it is atomic, so a crash leaves either nothing or a staging file the sweep collects.
#[derive(Debug)]
pub struct Staged {
    file: tokio::fs::File,
    path: std::path::PathBuf,
    written: u64,
    committed: bool,
}

impl Staged {
    #[must_use]
    pub const fn written(&self) -> u64 {
        self.written
    }

    /// Appends. The upload arrives in order and is never rewritten, so there is no offset
    /// for a caller to get wrong.
    pub async fn write(&mut self, bytes: &[u8]) -> Result<(), Error> {
        self.file.write_all(bytes).await.context(WriteSnafu { path: self.path.clone() })?;
        self.written += u64::try_from(bytes.len()).context(WidthSnafu)?;
        Ok(())
    }

    /// `fsync` the contents, rename into the content-addressed name, then `fsync` the
    /// directory that now names it. The rename is atomic within the filesystem, so the
    /// artifact appears whole or not at all, and the directory sync is what makes that
    /// appearance survive a power loss rather than only a process crash.
    ///
    /// A concurrent push of the same path races here and wins harmlessly: both writers
    /// produced byte-identical content, because the name is the hash of the content.
    pub async fn commit(
        mut self,
        store: &Store,
        url: &bincache_core::narurl::NarUrl,
    ) -> Result<u64, Error> {
        self.file.sync_all().await.context(SyncSnafu { path: self.path.clone() })?;

        let target = store.path(url);
        let shard = target.parent().unwrap_or(&target).to_path_buf();
        tokio::fs::create_dir_all(&shard).await.context(CreateDirSnafu { path: shard.clone() })?;
        tokio::fs::rename(&self.path, &target)
            .await
            .context(RenameSnafu { from: self.path.clone(), to: target })?;
        sync_dir(&shard).await?;

        self.committed = true;
        Ok(self.written)
    }

    /// Discards the upload. Callers on an error path should prefer this over relying on
    /// `Drop`, since it reports a failure rather than logging one.
    pub async fn abort(mut self) -> Result<(), Error> {
        self.committed = true;
        let removed = tokio::fs::remove_file(&self.path).await;
        if absent(&removed) {
            return Ok(());
        }
        removed.context(RemoveSnafu { path: self.path.clone() })
    }
}

/// A courtesy only. The guarantee that abandoned staging files go away is
/// [`Store::sweep_staging`] at boot, which is also what covers a crash.
impl Drop for Staged {
    fn drop(&mut self) {
        if !self.committed
            && let Err(error) = std::fs::remove_file(&self.path)
        {
            tracing::warn!(
                path = %self.path.display(),
                error = ?error,
                "staging file left for the boot sweep"
            );
        }
    }
}

/// `true` when the operation failed only because the target is not there.
fn absent<T>(result: &Result<T, std::io::Error>) -> bool {
    matches!(result, Err(source) if source.kind() == std::io::ErrorKind::NotFound)
}

/// One directory level, as owned paths. Blocking on purpose: this runs at boot and during
/// maintenance, never on the serving path.
fn entries(path: &std::path::Path) -> Result<Vec<std::path::PathBuf>, Error> {
    let listing = std::fs::read_dir(path).context(ListSnafu { path })?;
    let mut paths = Vec::new();
    for entry in listing {
        let entry = entry.context(ListSnafu { path })?;
        paths.push(entry.path());
    }
    Ok(paths)
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    fn url(seed: &[u8]) -> bincache_core::narurl::NarUrl {
        bincache_core::narurl::NarUrl {
            file_hash: bincache_core::hash::Sha256::digest(seed),
            compression: bincache_core::compression::Compression::Zstd,
        }
    }

    /// Each test gets its own root at a stable repo-owned path, so a failure leaves its
    /// artifacts where they can be inspected instead of in a scrubbed temporary directory.
    fn root(name: &str) -> std::path::PathBuf {
        let root = std::path::PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../../target"))
            .join("test-artifacts")
            .join("bincache-store")
            .join(name);
        if let Err(error) = std::fs::remove_dir_all(&root) {
            assert_eq!(error.kind(), std::io::ErrorKind::NotFound, "stale root not removable");
        }
        root
    }

    /// Everything a [`crate::nar::Reader`] has left to give, from `offset` on.
    async fn drain(reader: crate::nar::Reader, offset: u64) -> Vec<u8> {
        let mut file = reader.seek(offset).await.expect("seeks");
        let mut read = Vec::new();
        tokio::io::AsyncReadExt::read_to_end(&mut file, &mut read).await.expect("reads");
        read
    }

    #[tokio::test]
    async fn commits_an_upload_and_reads_it_back() {
        let store = crate::nar::Store::open(root("commit")).await.expect("opens");
        let url = url(b"payload");

        let mut staged = store.stage().await.expect("stages");
        staged.write(b"hello ").await.expect("writes");
        staged.write(b"world").await.expect("writes");
        assert_eq!(staged.written(), 11);
        assert_eq!(staged.commit(&store, &url).await.expect("commits"), 11);

        let reader = store.read(&url).await.expect("reads").expect("present");
        assert_eq!(reader.size(), 11);
        assert_eq!(drain(reader, 0).await, b"hello world");
    }

    /// Reading from an offset is how a resumed `Range` request is served, so the seek is
    /// asserted rather than assumed.
    #[tokio::test]
    async fn reads_from_an_offset() {
        let store = crate::nar::Store::open(root("offset")).await.expect("opens");
        let url = url(b"offset payload");

        let mut staged = store.stage().await.expect("stages");
        staged.write(b"hello world").await.expect("writes");
        staged.commit(&store, &url).await.expect("commits");

        let reader = store.read(&url).await.expect("reads").expect("present");
        assert_eq!(drain(reader, 6).await, b"world");
    }

    #[tokio::test]
    async fn reports_an_absent_artifact_rather_than_failing() {
        let store = crate::nar::Store::open(root("absent")).await.expect("opens");
        assert!(store.read(&url(b"missing")).await.expect("reads").is_none());
        assert_eq!(
            store.remove(&url(b"missing")).await.expect("removes"),
            crate::nar::Removed::Absent
        );
    }

    #[tokio::test]
    async fn an_abandoned_upload_never_appears_under_nar() {
        let store = crate::nar::Store::open(root("abandoned")).await.expect("opens");
        let url = url(b"abandoned");
        {
            let mut staged = store.stage().await.expect("stages");
            staged.write(b"partial").await.expect("writes");
            staged.abort().await.expect("aborts");
        }
        assert!(store.read(&url).await.expect("reads").is_none());
        assert_eq!(store.scan().expect("scans").artifacts.len(), 0);
        assert_eq!(store.sweep_staging().await.expect("sweeps"), 0);
    }

    #[tokio::test]
    async fn sweeps_staging_files_a_crash_left_behind() {
        let store = crate::nar::Store::open(root("sweep")).await.expect("opens");
        let mut staged = store.stage().await.expect("stages");
        staged.write(b"crashed").await.expect("writes");
        core::mem::forget(staged);

        assert_eq!(store.sweep_staging().await.expect("sweeps"), 1);
        assert_eq!(store.sweep_staging().await.expect("sweeps"), 0);
    }

    #[tokio::test]
    async fn scan_finds_what_commit_wrote() {
        let store = crate::nar::Store::open(root("scan")).await.expect("opens");
        let mut expected = Vec::new();
        for seed in [b"one".as_slice(), b"two".as_slice(), b"three".as_slice()] {
            let url = url(seed);
            let mut staged = store.stage().await.expect("stages");
            staged.write(b"x").await.expect("writes");
            staged.commit(&store, &url).await.expect("commits");
            expected.push(url);
        }

        let mut scan = store.scan().expect("scans");
        scan.artifacts.sort_by_key(|url| url.name());
        expected.sort_by_key(|url| url.name());
        assert_eq!(scan.artifacts, expected);
        assert!(scan.unrecognized.is_empty());
    }

    /// A delete during a stream must not break the stream: the descriptor holds the inode.
    #[tokio::test]
    async fn an_unlinked_artifact_still_streams() {
        let store = crate::nar::Store::open(root("unlink")).await.expect("opens");
        let url = url(b"unlinked");
        let mut staged = store.stage().await.expect("stages");
        staged.write(b"streaming").await.expect("writes");
        staged.commit(&store, &url).await.expect("commits");

        let reader = store.read(&url).await.expect("reads").expect("present");
        assert_eq!(store.remove(&url).await.expect("removes"), crate::nar::Removed::Deleted);

        assert_eq!(drain(reader, 0).await, b"streaming");
    }
}
