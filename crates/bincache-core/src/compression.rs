//! The `Compression:` narinfo field.
//!
//! A client picks its decompressor from this field, never from HTTP `Content-Encoding`.
//! An absent or empty field means bzip2 (`nix/src/libstore/nar-info.cc` defaults it that
//! way and calls the conditional a mistake in its own comment), which is why bincache
//! always emits the field explicitly.

#[derive(
    Clone,
    Copy,
    Debug,
    PartialEq,
    Eq,
    strum::AsRefStr,
    strum::Display,
    strum::EnumString,
    rkyv::Archive,
    rkyv::Serialize,
    rkyv::Deserialize,
)]
#[strum(serialize_all = "lowercase")]
pub enum Compression {
    None,
    Zstd,
    Xz,
    Bzip2,
    Br,
}

impl Compression {
    /// The suffix `nix/src/libstore/binary-cache-store.cc` appends to a NAR URL for this
    /// algorithm. A distinct mapping from the protocol name, not a second spelling of it.
    #[must_use]
    pub const fn extension(self) -> &'static str {
        match self {
            Self::None => "",
            Self::Zstd => ".zst",
            Self::Xz => ".xz",
            Self::Bzip2 => ".bz2",
            Self::Br => ".br",
        }
    }
}

#[cfg(test)]
mod tests {
    use core::str::FromStr as _;
    use pretty_assertions::assert_eq;

    #[test]
    fn renders_the_names_nix_writes() {
        assert_eq!(crate::compression::Compression::Zstd.to_string(), "zstd");
        assert_eq!(crate::compression::Compression::None.to_string(), "none");
        assert_eq!(crate::compression::Compression::Bzip2.to_string(), "bzip2");
    }

    #[test]
    fn parses_the_names_nix_writes() {
        let parsed = crate::compression::Compression::from_str("zstd").expect("known");
        assert_eq!(parsed, crate::compression::Compression::Zstd);
        assert!(crate::compression::Compression::from_str("lzma").is_err());
    }

    #[test]
    fn extensions_match_the_url_nix_builds() {
        assert_eq!(crate::compression::Compression::Zstd.extension(), ".zst");
        assert_eq!(crate::compression::Compression::None.extension(), "");
    }
}
