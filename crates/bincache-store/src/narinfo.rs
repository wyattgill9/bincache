//! Published narinfo bodies, as files holding exactly the bytes a `GET` returns.
//!
//! The record is rendered and signed once, at publish, from fields the server verified
//! itself. Nothing about it changes afterwards, so storing the finished body means serving
//! one is an `open` and a `write`: no transaction, no decode, no re-render, and no
//! allocation per reference.
//!
//! The rename into this directory is the publish. There is no second commit to keep in step
//! with it and nothing to replay at boot.

use snafu::ResultExt as _;

/// Subdirectory holding published bodies, keyed by store path hash.
const DIRECTORY: &str = "narinfo";

#[derive(Debug, snafu::Snafu)]
#[snafu(visibility(pub))]
pub enum Error {
    #[snafu(display("the narinfo directory could not be used"))]
    Dir { source: crate::atomic::Error },

    #[snafu(display("published file name {name:?} is not a store path hash"))]
    Name { name: String, source: bincache_core::storepath::Error },
}

/// The narinfo directory. Cheap to clone; every handler holds one.
#[derive(Clone, Debug)]
pub struct Store {
    dir: crate::atomic::Dir,
}

impl Store {
    pub async fn open(root: &std::path::Path) -> Result<Self, Error> {
        let dir = crate::atomic::Dir::open(root.join(DIRECTORY)).await.context(DirSnafu)?;
        Ok(Self { dir })
    }

    /// The body to answer a metadata request with. `None` is the common answer: closure
    /// resolution probes for paths the cache may not hold.
    /// Synchronous: see [`crate::atomic::Dir::read`] for why the hot metadata path does
    /// not go through a blocking threadpool.
    pub fn read(&self, key: &bincache_core::storepath::Hash) -> Result<Option<Vec<u8>>, Error> {
        self.dir.read(&name(key)).context(DirSnafu)
    }

    /// The publish. Everything expensive (compression, rendering, signing) already ran.
    pub async fn write(
        &self,
        key: &bincache_core::storepath::Hash,
        body: &str,
    ) -> Result<crate::atomic::Wrote, Error> {
        self.dir.write(&name(key), body.as_bytes()).await.context(DirSnafu)
    }

    pub async fn remove(
        &self,
        key: &bincache_core::storepath::Hash,
    ) -> Result<crate::atomic::Removed, Error> {
        self.dir.remove(&name(key)).await.context(DirSnafu)
    }

    /// Every published key, for reconciliation and for a signing-key rotation pass.
    ///
    /// A name that does not decode is an error rather than a skip: everything this
    /// directory holds was named by [`name`], so an undecodable one means something other
    /// than bincache wrote here.
    pub fn keys(&self) -> Result<Vec<bincache_core::storepath::Hash>, Error> {
        let listed = self.dir.list().context(DirSnafu)?;
        let mut keys = Vec::with_capacity(listed.len());
        for entry in listed {
            let text = entry.strip_suffix(bincache_core::narinfo::SUFFIX).unwrap_or(&entry);
            let key = bincache_core::storepath::Hash::parse(text)
                .context(NameSnafu { name: entry.clone() })?;
            keys.push(key);
        }
        Ok(keys)
    }

    /// How many paths the cache holds. One directory walk, so callers cache the answer
    /// rather than asking per request.
    pub fn count(&self) -> Result<u64, Error> {
        let listed = self.dir.list().context(DirSnafu)?;
        Ok(u64::try_from(listed.len()).unwrap_or(u64::MAX))
    }

    /// Clears temporary files a crash left behind.
    pub async fn sweep(&self) -> Result<usize, Error> {
        self.dir.sweep().await.context(DirSnafu)
    }
}

/// `<store path hash>.narinfo`, which is also the request target. Keeping the served name
/// and the stored name identical is what would let a static file server stand in front of
/// this directory unchanged.
fn name(key: &bincache_core::storepath::Hash) -> String {
    format!("{key}{}", bincache_core::narinfo::SUFFIX)
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    async fn store(name: &str) -> crate::narinfo::Store {
        let root = std::path::PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../../target"))
            .join("test-artifacts")
            .join("bincache-narinfo")
            .join(name);
        if let Err(error) = std::fs::remove_dir_all(&root) {
            assert_eq!(error.kind(), std::io::ErrorKind::NotFound, "stale root not removable");
        }
        crate::narinfo::Store::open(&root).await.expect("opens")
    }

    fn key(seed: &[u8]) -> bincache_core::storepath::Hash {
        let digest = bincache_core::hash::Sha256::digest(seed);
        let mut raw = [0u8; bincache_core::storepath::HASH_WIDTH];
        raw.copy_from_slice(&digest.as_bytes()[..bincache_core::storepath::HASH_WIDTH]);
        bincache_core::storepath::Hash::from_bytes(raw)
    }

    /// The stored bytes are the served bytes. Anything that trimmed or re-encoded them
    /// would break a client: the trailing space after `References:` is load-bearing.
    #[tokio::test]
    async fn serves_back_exactly_what_was_published() {
        let store = store("exact").await;
        let key = key(b"hello");
        let body = "StorePath: /nix/store/x\nReferences: \n";

        assert_eq!(store.read(&key).expect("reads"), None);
        assert_eq!(
            store.write(&key, body).await.expect("publishes"),
            crate::atomic::Wrote::Created
        );
        assert_eq!(
            store.read(&key).expect("reads"),
            Some(body.as_bytes().to_vec()),
            "the served body must be byte-identical to the published one"
        );
    }

    #[tokio::test]
    async fn republishing_a_path_is_idempotent() {
        let store = store("idempotent").await;
        let key = key(b"hello");
        store.write(&key, "first").await.expect("publishes");
        assert_eq!(
            store.write(&key, "second").await.expect("republishes"),
            crate::atomic::Wrote::Replaced
        );
        assert_eq!(store.count().expect("counts"), 1);
    }

    #[tokio::test]
    async fn lists_the_keys_it_published() {
        let store = store("keys").await;
        let mut expected: Vec<bincache_core::storepath::Hash> = Vec::new();
        for seed in [b"one".as_slice(), b"two".as_slice(), b"three".as_slice()] {
            let key = key(seed);
            store.write(&key, "body").await.expect("publishes");
            expected.push(key);
        }

        let mut keys = store.keys().expect("lists");
        keys.sort_by_key(bincache_core::storepath::Hash::to_string);
        expected.sort_by_key(bincache_core::storepath::Hash::to_string);
        assert_eq!(keys, expected);
        assert_eq!(store.count().expect("counts"), 3);
    }

    #[tokio::test]
    async fn removing_a_path_forgets_it() {
        let store = store("remove").await;
        let key = key(b"hello");
        store.write(&key, "body").await.expect("publishes");

        assert_eq!(store.remove(&key).await.expect("removes"), crate::atomic::Removed::Deleted);
        assert_eq!(store.read(&key).expect("reads"), None);
        assert_eq!(store.count().expect("counts"), 0);
        assert_eq!(store.remove(&key).await.expect("removes"), crate::atomic::Removed::Absent);
    }
}
