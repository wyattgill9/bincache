//! SHA-256 digests, the only hash the binary cache protocol carries on the wire.
//!
//! `NarHash` describes the uncompressed NAR bytes and `FileHash` describes the compressed
//! artifact that names the file on disk. Both are this type; which one a value is comes
//! from the field that holds it, not from a wrapper.

use sha2::Digest as _;
use snafu::ResultExt as _;

/// Bytes in a SHA-256 digest.
pub const WIDTH: usize = 32;

/// Characters in the nix-base32 rendering of a SHA-256 digest.
pub const TEXT_LEN: usize = crate::base32::text_len(WIDTH);

/// Characters in the lowercase hex rendering.
const HEX_LEN: usize = WIDTH * 2;

/// Characters in the padded standard-base64 rendering.
const BASE64_LEN: usize = 44;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Sha256([u8; WIDTH]);

#[derive(Debug, snafu::Snafu)]
#[snafu(visibility(pub))]
pub enum Error {
    #[snafu(display("nix-base32 digest is malformed"))]
    Base32 { source: crate::base32::Error },

    #[snafu(display("hex digest is malformed"))]
    Hex { source: data_encoding::DecodeError },

    #[snafu(display("base64 digest is malformed"))]
    Base64 { source: data_encoding::DecodeError },

    #[snafu(display("decoded digest is not {WIDTH} bytes"))]
    Width { source: core::array::TryFromSliceError },

    #[snafu(display(
        "digest text is {found} characters, expected {TEXT_LEN} (base32), \
         {HEX_LEN} (hex), or {BASE64_LEN} (base64)"
    ))]
    Encoding { found: usize },
}

impl Sha256 {
    #[must_use]
    pub const fn from_bytes(bytes: [u8; WIDTH]) -> Self {
        Self(bytes)
    }

    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; WIDTH] {
        &self.0
    }

    #[must_use]
    pub fn digest(bytes: &[u8]) -> Self {
        let mut hasher = Hasher::default();
        hasher.update(bytes);
        hasher.finish()
    }

    /// The bare nix-base32 body, with no `sha256:` prefix. This is what names a NAR file
    /// and what appears inside a `URL:` line.
    #[must_use]
    pub fn base32(&self) -> String {
        crate::base32::encode(&self.0)
    }

    /// Accepts every spelling a Nix client may emit: an optional `sha256:` or SRI
    /// `sha256-` prefix in front of nix-base32, lowercase hex, or padded base64.
    pub fn parse(text: &str) -> Result<Self, Error> {
        let body =
            text.strip_prefix("sha256:").or_else(|| text.strip_prefix("sha256-")).unwrap_or(text);

        match body.len() {
            TEXT_LEN => {
                let bytes: [u8; WIDTH] = crate::base32::decode(body).context(Base32Snafu)?;
                Ok(Self(bytes))
            }
            HEX_LEN => Self::from_slice(
                &data_encoding::HEXLOWER_PERMISSIVE.decode(body.as_bytes()).context(HexSnafu)?,
            ),
            BASE64_LEN => Self::from_slice(
                &data_encoding::BASE64.decode(body.as_bytes()).context(Base64Snafu)?,
            ),
            found => EncodingSnafu { found }.fail(),
        }
    }

    fn from_slice(bytes: &[u8]) -> Result<Self, Error> {
        let sized: [u8; WIDTH] = bytes.try_into().context(WidthSnafu)?;
        Ok(Self(sized))
    }
}

/// The canonical wire rendering, `sha256:<nix-base32>`, which is what `NarHash:` and
/// `FileHash:` lines carry.
impl core::fmt::Display for Sha256 {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(formatter, "sha256:{}", self.base32())
    }
}

/// Incremental digest, so ingest never holds a whole NAR in memory to hash it.
#[derive(Clone, Default)]
pub struct Hasher(sha2::Sha256);

impl Hasher {
    pub fn update(&mut self, bytes: &[u8]) {
        self.0.update(bytes);
    }

    #[must_use]
    pub fn finish(self) -> Sha256 {
        Sha256(self.0.finalize().into())
    }
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    const EMPTY: &str = "sha256:0mdqa9w1p6cmli6976v4wi0sw9r4p5prkj7lzfd1877wk11c9c73";

    #[test]
    fn digests_the_empty_input_like_nix() {
        assert_eq!(crate::hash::Sha256::digest(b"").to_string(), EMPTY);
    }

    #[test]
    fn incremental_matches_one_shot() {
        let mut hasher = crate::hash::Hasher::default();
        hasher.update(b"nix-archive-1");
        hasher.update(b"(");
        assert_eq!(hasher.finish(), crate::hash::Sha256::digest(b"nix-archive-1("));
    }

    #[test]
    fn parses_every_accepted_spelling() {
        let expected = crate::hash::Sha256::digest(b"");
        let hex = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
        let base64 = "47DEQpj8HBSa+/TImW+5JCeuQeRkm5NMpJWZG3hSuFU=";
        for text in [EMPTY, &EMPTY[7..], hex, base64, &format!("sha256-{base64}")] {
            assert_eq!(crate::hash::Sha256::parse(text).expect("parses"), expected);
        }
    }

    #[test]
    fn rejects_an_unknown_length() {
        assert!(matches!(
            crate::hash::Sha256::parse("sha256:abc"),
            Err(crate::hash::Error::Encoding { found: 3 })
        ));
    }
}
