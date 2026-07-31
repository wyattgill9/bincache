//! Where the artifact for a given NAR hash ended up.
//!
//! Written when a `PUT` of a NAR commits, read for two things: the `HEAD` a client sends
//! against the NAR URL before deciding to upload, and the narinfo `PUT` that arrives next
//! and needs to know which artifact its record should point at.

/// The NAR hash a client declares is over the *uncompressed* bytes, and bincache
/// re-compresses on receipt, so the artifact is named by a hash the client never computed.
/// This is the mapping between the two.
#[derive(Clone, Copy, Debug, PartialEq, Eq, rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)]
pub struct Entry {
    /// Over the stored artifact. Names the file, and therefore the served URL.
    pub file_hash: bincache_core::hash::Sha256,
    pub file_size: u64,
    /// Over the uncompressed NAR. Carried so a narinfo `PUT` can be checked against what
    /// was actually received rather than against what it claims.
    pub nar_size: core::num::NonZeroU64,
    pub compression: bincache_core::compression::Compression,
}

impl Entry {
    #[must_use]
    pub const fn url(&self) -> bincache_core::narurl::NarUrl {
        bincache_core::narurl::NarUrl { file_hash: self.file_hash, compression: self.compression }
    }
}
