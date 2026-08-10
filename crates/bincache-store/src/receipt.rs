//! What happened to the NAR with a given hash.
//!
//! A client declares the hash of the *uncompressed* NAR and bincache recompresses on
//! receipt, so the artifact on disk is named by a hash the client never computed. This is
//! the mapping between the two, and it is written when a `PUT` of a NAR commits.
//!
//! Two things read it. The `HEAD nar/<nar hash>.nar` a client sends before deciding to
//! upload, and the narinfo `PUT` that arrives next and needs to know which artifact its
//! record should point at.
//!
//! `nar_size` is here because it is the one field that cannot be recovered from the
//! filesystem: it describes the uncompressed bytes, and a streamed zstd frame does not
//! pledge its content size.

use snafu::OptionExt as _;
use snafu::ResultExt as _;

/// Subdirectory holding receipts, keyed by NAR hash.
const DIRECTORY: &str = "bynar";

#[derive(Debug, snafu::Snafu)]
#[snafu(visibility(pub))]
pub enum Error {
    #[snafu(display("the receipt directory could not be used"))]
    Dir { source: crate::atomic::Error },

    #[snafu(display("a receipt has {found} fields, expected 3"))]
    Fields { found: usize },

    #[snafu(display("a receipt does not name a sha256 digest"))]
    Digest { source: bincache_core::hash::Error },

    #[snafu(display("a receipt size is not a number"))]
    Size { source: core::num::ParseIntError },

    #[snafu(display("a receipt declares a zero NarSize, which no upload can produce"))]
    Empty,

    #[snafu(display("a receipt is not UTF-8; something other than bincache wrote it"))]
    Encoding { source: core::str::Utf8Error },
}

/// Everything the two readers need, and nothing that can be derived.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Receipt {
    /// Over the stored artifact. Names the file, and therefore the served URL.
    pub file_hash: bincache_core::hash::Sha256,
    pub file_size: u64,
    /// Over the uncompressed NAR, which is what the protocol's `NarSize` reports.
    pub nar_size: core::num::NonZeroU64,
}

impl Receipt {
    /// Where the artifact this receipt describes lives. Derived rather than stored: the
    /// compression is a property of the build, not of the upload.
    #[must_use]
    pub const fn url(&self) -> bincache_core::narurl::NarUrl {
        bincache_core::narurl::NarUrl {
            file_hash: self.file_hash,
            compression: bincache_core::compression::STORED,
        }
    }

    /// One line, three fields. A format an operator can read with `cat` and this module can
    /// parse without a dependency.
    #[must_use]
    pub fn render(&self) -> String {
        format!("{} {} {}\n", self.file_hash.base32(), self.file_size, self.nar_size)
    }

    pub fn parse(text: &str) -> Result<Self, Error> {
        let mut fields = text.split_whitespace();
        let file_hash = fields.next().context(FieldsSnafu { found: 0usize })?;
        let file_size = fields.next().context(FieldsSnafu { found: 1usize })?;
        let nar_size = fields.next().context(FieldsSnafu { found: 2usize })?;
        let extra = fields.count();
        snafu::ensure!(extra == 0, FieldsSnafu { found: 3 + extra });

        let file_hash = bincache_core::hash::Sha256::parse(file_hash).context(DigestSnafu)?;
        let file_size: u64 = file_size.parse().context(SizeSnafu)?;
        let nar_size: u64 = nar_size.parse().context(SizeSnafu)?;
        let nar_size = core::num::NonZeroU64::new(nar_size).context(EmptySnafu)?;
        Ok(Self { file_hash, file_size, nar_size })
    }
}

/// The receipt directory. Cheap to clone; every handler holds one.
#[derive(Clone, Debug)]
pub struct Store {
    dir: crate::atomic::Dir,
}

impl Store {
    pub async fn open(root: &std::path::Path) -> Result<Self, Error> {
        let dir = crate::atomic::Dir::open(root.join(DIRECTORY)).await.context(DirSnafu)?;
        Ok(Self { dir })
    }

    pub async fn read(
        &self,
        nar_hash: &bincache_core::hash::Sha256,
    ) -> Result<Option<Receipt>, Error> {
        let Some(raw) = self.dir.read(&nar_hash.base32()).await.context(DirSnafu)? else {
            return Ok(None);
        };
        let text = core::str::from_utf8(&raw).context(EncodingSnafu)?;
        Receipt::parse(text).map(Some)
    }

    pub async fn write(
        &self,
        nar_hash: &bincache_core::hash::Sha256,
        receipt: &Receipt,
    ) -> Result<(), Error> {
        self.dir.write(&nar_hash.base32(), receipt.render().as_bytes()).await.context(DirSnafu)?;
        Ok(())
    }

    pub async fn remove(
        &self,
        nar_hash: &bincache_core::hash::Sha256,
    ) -> Result<crate::atomic::Removed, Error> {
        self.dir.remove(&nar_hash.base32()).await.context(DirSnafu)
    }

    /// Clears temporary files a crash left behind.
    pub async fn sweep(&self) -> Result<usize, Error> {
        self.dir.sweep().await.context(DirSnafu)
    }
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    fn sample() -> crate::receipt::Receipt {
        crate::receipt::Receipt {
            file_hash: bincache_core::hash::Sha256::digest(b"artifact"),
            file_size: 512,
            nar_size: core::num::NonZeroU64::new(2048).expect("nonzero"),
        }
    }

    #[test]
    fn round_trips_through_its_own_format() {
        let receipt = sample();
        assert_eq!(crate::receipt::Receipt::parse(&receipt.render()).expect("parses"), receipt);
    }

    #[test]
    fn renders_one_readable_line() {
        let rendered = sample().render();
        expect_test::expect![[r#"
            076vrba992s2v5zgkh8xkg141vr3nb8ayn31mcb49v2x1kbw3if7 512 2048
        "#]]
        .assert_eq(&rendered);
    }

    #[test]
    fn refuses_a_receipt_it_cannot_trust() {
        // A short line reports how many fields were actually there, so a corrupt receipt
        // says what is wrong with it rather than which parse step noticed.
        assert!(matches!(
            crate::receipt::Receipt::parse(""),
            Err(crate::receipt::Error::Fields { found: 0 })
        ));
        assert!(matches!(
            crate::receipt::Receipt::parse("only-one-field"),
            Err(crate::receipt::Error::Fields { found: 1 })
        ));
        assert!(matches!(
            crate::receipt::Receipt::parse("a b c d"),
            Err(crate::receipt::Error::Fields { found: 4 })
        ));
        assert!(matches!(
            crate::receipt::Receipt::parse("not-a-hash 512 2048"),
            Err(crate::receipt::Error::Digest { .. })
        ));
        assert!(matches!(
            crate::receipt::Receipt::parse(&format!(
                "{} huge 2048",
                bincache_core::hash::Sha256::digest(b"x").base32()
            )),
            Err(crate::receipt::Error::Size { .. })
        ));

        // A zero NarSize is unrepresentable rather than merely wrong: nix throws `corrupt`
        // on one, and ingest refuses an empty upload, so reading one back means the file
        // was not written by this cache.
        let zeroed = format!("{} 512 0\n", bincache_core::hash::Sha256::digest(b"x").base32());
        assert!(matches!(
            crate::receipt::Receipt::parse(&zeroed),
            Err(crate::receipt::Error::Empty)
        ));
    }

    /// The URL is derived, so a receipt cannot disagree with the file it names.
    #[test]
    fn names_the_artifact_it_describes() {
        let receipt = sample();
        assert_eq!(receipt.url().name(), format!("{}.nar.zst", receipt.file_hash.base32()));
    }

    #[tokio::test]
    async fn survives_a_round_trip_through_the_directory() {
        let root = std::path::PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../../target"))
            .join("test-artifacts")
            .join("bincache-receipt");
        if let Err(error) = std::fs::remove_dir_all(&root) {
            assert_eq!(error.kind(), std::io::ErrorKind::NotFound, "stale root not removable");
        }
        let store = crate::receipt::Store::open(&root).await.expect("opens");
        let nar_hash = bincache_core::hash::Sha256::digest(b"uncompressed");

        assert_eq!(store.read(&nar_hash).await.expect("reads"), None);
        store.write(&nar_hash, &sample()).await.expect("writes");
        assert_eq!(store.read(&nar_hash).await.expect("reads"), Some(sample()));

        assert_eq!(
            store.remove(&nar_hash).await.expect("removes"),
            crate::atomic::Removed::Deleted
        );
        assert_eq!(store.read(&nar_hash).await.expect("reads"), None);
    }
}
