//! The `nar/<file hash>.nar<ext>` naming convention, in both directions.
//!
//! One owner, because four places need it and they must agree: `NarInfo::url` renders it,
//! `bincache-store` derives a filename from it, `bincache-serve` routes on it, and the
//! `PUT` path reads the expected hash straight out of the request target.

use snafu::OptionExt as _;
use snafu::ResultExt as _;

/// Directory component every NAR URL starts with.
pub const PREFIX: &str = "nar/";

/// Content type for a NAR body, per `nix/src/libstore/binary-cache-store.cc`.
pub const CONTENT_TYPE: &str = "application/x-nix-nar";

/// Everything a NAR URL says: which artifact, and how it is compressed. The compression is
/// in the extension rather than a header, because a client picks its decompressor from the
/// narinfo `Compression` field and never from HTTP `Content-Encoding`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NarUrl {
    pub file_hash: crate::hash::Sha256,
    pub compression: crate::compression::Compression,
}

#[derive(Debug, snafu::Snafu)]
#[snafu(visibility(pub))]
pub enum Error {
    #[snafu(display("NAR name {name:?} has no `.nar` component"))]
    Suffix { name: String },

    #[snafu(display("NAR name extension {extension:?} names no algorithm this build knows"))]
    Extension { extension: String },

    #[snafu(display("NAR name does not start with a sha256 digest"))]
    Digest { source: crate::hash::Error },
}

impl NarUrl {
    /// `<file hash>.nar<ext>`: the file name, with no directory.
    #[must_use]
    pub fn name(&self) -> String {
        format!("{}.nar{}", self.file_hash.base32(), self.compression.extension())
    }

    /// `nar/<file hash>.nar<ext>`: the request target and the `URL:` field.
    #[must_use]
    pub fn url(&self) -> String {
        format!("{PREFIX}{}", self.name())
    }

    /// Reads a name back, with no directory component. The inverse of [`NarUrl::name`].
    pub fn parse(name: &str) -> Result<Self, Error> {
        let (digest, extension) = name.split_once(".nar").context(SuffixSnafu { name })?;
        let compression = crate::compression::Compression::from_extension(extension)
            .context(ExtensionSnafu { extension })?;
        let file_hash = crate::hash::Sha256::parse(digest).context(DigestSnafu)?;
        Ok(Self { file_hash, compression })
    }
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    fn sample(compression: crate::compression::Compression) -> crate::narurl::NarUrl {
        crate::narurl::NarUrl { file_hash: crate::hash::Sha256::digest(b"nar"), compression }
    }

    #[test]
    fn round_trips_every_compression() {
        for compression in [
            crate::compression::Compression::None,
            crate::compression::Compression::Zstd,
            crate::compression::Compression::Xz,
            crate::compression::Compression::Bzip2,
            crate::compression::Compression::Br,
        ] {
            let url = sample(compression);
            assert_eq!(crate::narurl::NarUrl::parse(&url.name()).expect("parses"), url);
        }
    }

    #[test]
    fn renders_the_url_nix_builds() {
        let url = sample(crate::compression::Compression::Zstd);
        assert_eq!(url.url(), "nar/0gsyc3g0w8wacg97wwm1iirigsl96k36iijdxiadn8cqcjmx18y1.nar.zst");
    }

    /// `?compression=none` uploads land here: the name is the bare NAR hash, so the `PUT`
    /// target states what the body must hash to.
    #[test]
    fn parses_an_uncompressed_upload_target() {
        let parsed = crate::narurl::NarUrl::parse(
            "0gsyc3g0w8wacg97wwm1iirigsl96k36iijdxiadn8cqcjmx18y1.nar",
        )
        .expect("parses");
        assert_eq!(parsed.compression, crate::compression::Compression::None);
        assert_eq!(parsed.file_hash, crate::hash::Sha256::digest(b"nar"));
    }

    #[test]
    fn rejects_a_name_without_a_nar_component() {
        assert!(matches!(
            crate::narurl::NarUrl::parse("whatever.tar.gz"),
            Err(crate::narurl::Error::Suffix { .. })
        ));
    }

    #[test]
    fn rejects_an_unknown_extension() {
        let name = "0gsyc3g0w8wacg97wwm1iirigsl96k36iijdxiadn8cqcjmx18y1.nar.lzma";
        assert!(matches!(
            crate::narurl::NarUrl::parse(name),
            Err(crate::narurl::Error::Extension { .. })
        ));
    }
}
