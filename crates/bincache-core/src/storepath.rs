//! Store paths: the 20-byte primary key of the whole protocol, the base name it prefixes,
//! and the store directory those names hang under.
//!
//! `research/NIX_PRIMER.md` has the derivation of the 20 bytes. What matters here is that
//! the request key `<32 base32 characters>.narinfo` decodes to exactly this type at the
//! socket, so nothing downstream can hold an unvalidated one.

use snafu::OptionExt as _;
use snafu::ResultExt as _;

/// Bytes in a store path hash: a SHA-256 folded in half by XOR (`compressHash`).
pub const HASH_WIDTH: usize = 20;

/// Characters in the base32 rendering. 160 bits at 5 bits each, so no padding.
pub const HASH_TEXT_LEN: usize = crate::base32::text_len(HASH_WIDTH);

/// `StorePath::MaxPathLen` in `nix/src/libstore/path.hh`.
const NAME_LEN_MAX: usize = 211;

/// The default store directory. Declared here rather than duplicated at every call site;
/// the running configuration always passes its own value.
pub const DIR_DEFAULT: &str = "/nix/store";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Hash([u8; HASH_WIDTH]);

/// The part of a base name after the hash and its separating `-`.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Name(String);

/// A base name, `<hash>-<name>`. The store directory is not part of it, exactly as in
/// `nix/src/libstore/path.hh`, so a record never repeats the directory per reference.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Path {
    hash: Hash,
    name: Name,
}

/// An absolute store directory with no trailing slash.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Dir(String);

#[derive(Debug, snafu::Snafu)]
#[snafu(visibility(pub))]
pub enum Error {
    #[snafu(display("store path hash is not {HASH_TEXT_LEN} base32 characters"))]
    HashText { source: crate::base32::Error },

    #[snafu(display("base name {name:?} has no `-` after its {HASH_TEXT_LEN}-character hash"))]
    Separator { name: String },

    #[snafu(display("store path name is empty"))]
    NameEmpty,

    #[snafu(display("store path name is {found} characters, over the {NAME_LEN_MAX} limit"))]
    NameLength { found: usize },

    #[snafu(display("store path name starts with `.`"))]
    NameLeadingDot,

    #[snafu(display("byte {byte:#04x} is not legal in a store path name"))]
    NameCharacter { byte: u8 },

    #[snafu(display("store directory {text:?} is not an absolute path"))]
    DirRelative { text: String },

    #[snafu(display("store directory {text:?} has a trailing slash"))]
    DirTrailingSlash { text: String },

    #[snafu(display("{text:?} is not under store directory {dir:?}"))]
    DirPrefix { text: String, dir: String },
}

impl Hash {
    #[must_use]
    pub const fn from_bytes(bytes: [u8; HASH_WIDTH]) -> Self {
        Self(bytes)
    }

    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; HASH_WIDTH] {
        &self.0
    }

    /// Decodes the fixed-width request key. On a public endpoint this doubles as input
    /// validation: anything malformed dies here, before any lookup.
    pub fn parse(text: &str) -> Result<Self, Error> {
        let bytes: [u8; HASH_WIDTH] = crate::base32::decode(text).context(HashTextSnafu)?;
        Ok(Self(bytes))
    }
}

impl core::fmt::Display for Hash {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter.write_str(&crate::base32::encode(&self.0))
    }
}

impl Name {
    pub fn new(text: String) -> Result<Self, Error> {
        snafu::ensure!(!text.is_empty(), NameEmptySnafu);
        snafu::ensure!(text.len() <= NAME_LEN_MAX, NameLengthSnafu { found: text.len() });
        snafu::ensure!(!text.starts_with('.'), NameLeadingDotSnafu);
        for byte in text.bytes() {
            let punctuation = matches!(byte, b'+' | b'-' | b'.' | b'_' | b'?' | b'=');
            snafu::ensure!(
                byte.is_ascii_alphanumeric() || punctuation,
                NameCharacterSnafu { byte }
            );
        }
        Ok(Self(text))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl core::fmt::Display for Name {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl Path {
    #[must_use]
    pub const fn new(hash: Hash, name: Name) -> Self {
        Self { hash, name }
    }

    #[must_use]
    pub const fn hash(&self) -> &Hash {
        &self.hash
    }

    #[must_use]
    pub const fn name(&self) -> &Name {
        &self.name
    }

    /// Parses a base name with no directory, which is the form `References:` carries.
    pub fn parse(text: &str) -> Result<Self, Error> {
        snafu::ensure!(
            text.len() > HASH_TEXT_LEN && text.as_bytes()[HASH_TEXT_LEN] == b'-',
            SeparatorSnafu { name: text.to_owned() }
        );
        let (hash, rest) = text.split_at(HASH_TEXT_LEN);
        Ok(Self { hash: Hash::parse(hash)?, name: Name::new(rest[1..].to_owned())? })
    }
}

impl core::fmt::Display for Path {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(formatter, "{}-{}", self.hash, self.name)
    }
}

impl Dir {
    pub fn new(text: String) -> Result<Self, Error> {
        snafu::ensure!(text.starts_with('/'), DirRelativeSnafu { text: text.clone() });
        snafu::ensure!(!text.ends_with('/'), DirTrailingSlashSnafu { text: text.clone() });
        Ok(Self(text))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// `/nix/store/<hash>-<name>`, the form every `StorePath:`, `Deriver:`, and fingerprint
    /// reference is written in.
    #[must_use]
    pub fn print(&self, path: &Path) -> String {
        format!("{}/{path}", self.0)
    }

    pub fn parse(&self, text: &str) -> Result<Path, Error> {
        let base = text
            .strip_prefix(&self.0)
            .and_then(|rest| rest.strip_prefix('/'))
            .context(DirPrefixSnafu { text, dir: self.0.clone() })?;
        Path::parse(base)
    }
}

impl core::fmt::Display for Dir {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter.write_str(&self.0)
    }
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    const HELLO: &str = "5rnvz1n7hdmvbdzq0d5m5xrz3xz6ky8j-hello-2.12.1";

    #[test]
    fn round_trips_a_real_base_name() {
        let path = crate::storepath::Path::parse(HELLO).expect("parses");
        assert_eq!(path.name().as_str(), "hello-2.12.1");
        assert_eq!(path.to_string(), HELLO);
    }

    #[test]
    fn prints_and_reparses_under_a_directory() {
        let dir = crate::storepath::Dir::new("/nix/store".to_owned()).expect("absolute");
        let path = crate::storepath::Path::parse(HELLO).expect("parses");
        let printed = dir.print(&path);
        assert_eq!(printed, format!("/nix/store/{HELLO}"));
        assert_eq!(dir.parse(&printed).expect("reparses"), path);
    }

    #[test]
    fn rejects_a_foreign_store_directory() {
        let dir = crate::storepath::Dir::new("/nix/store".to_owned()).expect("absolute");
        assert!(matches!(
            dir.parse(&format!("/other/store/{HELLO}")),
            Err(crate::storepath::Error::DirPrefix { .. })
        ));
    }

    #[test]
    fn rejects_a_missing_separator() {
        let text = "5rnvz1n7hdmvbdzq0d5m5xrz3xz6ky8jhello";
        assert!(matches!(
            crate::storepath::Path::parse(text),
            Err(crate::storepath::Error::Separator { .. })
        ));
    }

    #[test]
    fn rejects_an_illegal_name_byte() {
        assert!(matches!(
            crate::storepath::Name::new("hello world".to_owned()),
            Err(crate::storepath::Error::NameCharacter { byte: b' ' })
        ));
    }

    #[test]
    fn rejects_a_relative_store_directory() {
        assert!(matches!(
            crate::storepath::Dir::new("nix/store".to_owned()),
            Err(crate::storepath::Error::DirRelative { .. })
        ));
    }
}
