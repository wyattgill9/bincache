//! The narinfo record: the object the whole read path exists to return.
//!
//! Stored as fields rather than as rendered bytes, per `research/DESIGN_V2.md`
//! ("What gets prerendered"), so the render format can change and the signing key can
//! rotate without a data migration. Rendering produces a **body**; framing belongs to
//! `bincache-serve`.

pub mod parse;

use swrite::SWrite as _;

/// Content type for a rendered body, per `nix/src/libstore/binary-cache-store.cc`.
pub const CONTENT_TYPE: &str = "text/x-nix-narinfo";

/// Suffix of the metadata request key.
pub const SUFFIX: &str = ".narinfo";

#[derive(Clone, Debug, PartialEq, Eq, rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)]
pub struct NarInfo {
    pub store_path: crate::storepath::Path,
    pub compression: crate::compression::Compression,
    /// Over the compressed artifact. Names the file on disk and therefore the URL.
    pub file_hash: crate::hash::Sha256,
    pub file_size: u64,
    /// Over the uncompressed NAR bytes. The client's post-decompression check.
    pub nar_hash: crate::hash::Sha256,
    /// Nonzero by protocol: `NarInfo::NarInfo` throws `corrupt` on zero, and
    /// `ValidPathInfo::fingerprint` refuses to build a fingerprint without it.
    pub nar_size: core::num::NonZeroU64,
    pub references: Vec<crate::storepath::Path>,
    pub deriver: Option<crate::storepath::Path>,
    pub sigs: Vec<crate::sign::Signature>,
    pub ca: Option<String>,
}

impl NarInfo {
    /// Where the artifact this record describes lives. Derived rather than stored: the
    /// record already names both halves, and a second copy would be a second thing to keep
    /// coherent.
    #[must_use]
    pub fn nar(&self) -> crate::narurl::NarUrl {
        crate::narurl::NarUrl { file_hash: self.file_hash, compression: self.compression }
    }

    /// The canonical string ed25519 signs, per `ValidPathInfo::fingerprint`. References are
    /// printed as full paths in the sorted order `printStorePathSet` produces, which is
    /// lexicographic by base name.
    #[must_use]
    pub fn fingerprint(&self, dir: &crate::storepath::Dir) -> String {
        let mut sorted: Vec<String> =
            self.references.iter().map(|reference| dir.print(reference)).collect();
        sorted.sort_unstable();

        let mut text = String::with_capacity(128 + sorted.len() * 64);
        swrite::swrite!(text, "1;{};", dir.print(&self.store_path));
        swrite::swrite!(text, "{};{};", self.nar_hash, self.nar_size);
        text.push_str(&sorted.join(","));
        text
    }

    /// Replaces every signature with one from `key`. Used at publish and by a key rotation
    /// pass, which is a background walk over the records with no client-visible break.
    pub fn resign(&mut self, dir: &crate::storepath::Dir, key: &crate::sign::SecretKey) {
        let signature = key.sign(&self.fingerprint(dir));
        self.sigs.clear();
        self.sigs.push(signature);
    }

    /// The body a `GET /<hash>.narinfo` answers with. Field order matches
    /// `NarInfo::to_string` so a byte-diff against nix's own output stays meaningful.
    #[must_use]
    pub fn render(&self, dir: &crate::storepath::Dir) -> String {
        let mut body = String::with_capacity(1024);
        swrite::swriteln!(body, "StorePath: {}", dir.print(&self.store_path));
        swrite::swriteln!(body, "URL: {}", self.nar().url());
        swrite::swriteln!(body, "Compression: {}", self.compression);
        swrite::swriteln!(body, "FileHash: {}", self.file_hash);
        swrite::swriteln!(body, "FileSize: {}", self.file_size);
        swrite::swriteln!(body, "NarHash: {}", self.nar_hash);
        swrite::swriteln!(body, "NarSize: {}", self.nar_size);

        body.push_str("References:");
        for reference in &self.references {
            swrite::swrite!(body, " {reference}");
        }
        body.push('\n');

        if let Some(deriver) = &self.deriver {
            swrite::swriteln!(body, "Deriver: {deriver}");
        }
        for signature in &self.sigs {
            swrite::swriteln!(body, "Sig: {signature}");
        }
        if let Some(ca) = &self.ca {
            swrite::swriteln!(body, "CA: {ca}");
        }
        body
    }
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    fn dir() -> crate::storepath::Dir {
        crate::storepath::Dir::new(crate::storepath::DIR_DEFAULT.to_owned()).expect("absolute")
    }

    pub(crate) fn sample() -> crate::narinfo::NarInfo {
        crate::narinfo::NarInfo {
            store_path: crate::storepath::Path::parse(
                "5rnvz1n7hdmvbdzq0d5m5xrz3xz6ky8j-hello-2.12.1",
            )
            .expect("parses"),
            compression: crate::compression::Compression::Zstd,
            file_hash: crate::hash::Sha256::digest(b"compressed"),
            file_size: 50088,
            nar_hash: crate::hash::Sha256::digest(b"nar"),
            nar_size: core::num::NonZeroU64::new(226504).expect("nonzero"),
            references: vec![
                crate::storepath::Path::parse("5rnvz1n7hdmvbdzq0d5m5xrz3xz6ky8j-hello-2.12.1")
                    .expect("parses"),
                crate::storepath::Path::parse("kzp5qfy8m2h3vqcvjrsw1sn8fzsz5nx8-glibc-2.37")
                    .expect("parses"),
            ],
            deriver: crate::storepath::Path::parse(
                "9fs4vq4gdsb8r9ywawq5c9dfvj7lp5g8-hello-2.12.1.drv",
            )
            .ok(),
            sigs: Vec::new(),
            ca: None,
        }
    }

    #[test]
    fn renders_the_fields_nix_writes_in_order() {
        expect_test::expect![[r#"
            StorePath: /nix/store/5rnvz1n7hdmvbdzq0d5m5xrz3xz6ky8j-hello-2.12.1
            URL: nar/014q9y1r70rmmayxc8ggydvw8wsnzjsqih7m5nksycxwwk10i8wx.nar.zst
            Compression: zstd
            FileHash: sha256:014q9y1r70rmmayxc8ggydvw8wsnzjsqih7m5nksycxwwk10i8wx
            FileSize: 50088
            NarHash: sha256:0gsyc3g0w8wacg97wwm1iirigsl96k36iijdxiadn8cqcjmx18y1
            NarSize: 226504
            References: 5rnvz1n7hdmvbdzq0d5m5xrz3xz6ky8j-hello-2.12.1 kzp5qfy8m2h3vqcvjrsw1sn8fzsz5nx8-glibc-2.37
            Deriver: 9fs4vq4gdsb8r9ywawq5c9dfvj7lp5g8-hello-2.12.1.drv
        "#]]
        .assert_eq(&sample().render(&dir()));
    }

    #[test]
    fn emits_an_empty_references_line() {
        let mut info = sample();
        info.references.clear();
        assert!(info.render(&dir()).contains("\nReferences:\n"));
    }

    #[test]
    fn fingerprint_matches_the_documented_shape() {
        expect_test::expect!["1;/nix/store/5rnvz1n7hdmvbdzq0d5m5xrz3xz6ky8j-hello-2.12.1;sha256:0gsyc3g0w8wacg97wwm1iirigsl96k36iijdxiadn8cqcjmx18y1;226504;/nix/store/5rnvz1n7hdmvbdzq0d5m5xrz3xz6ky8j-hello-2.12.1,/nix/store/kzp5qfy8m2h3vqcvjrsw1sn8fzsz5nx8-glibc-2.37"]
        .assert_eq(&sample().fingerprint(&dir()));
    }

    #[test]
    fn fingerprint_sorts_references_regardless_of_record_order() {
        let mut info = sample();
        info.references.reverse();
        assert_eq!(info.fingerprint(&dir()), sample().fingerprint(&dir()));
    }

    #[test]
    fn resign_replaces_rather_than_appends() {
        let key = crate::sign::SecretKey::generate("bincache-test-1".to_owned());
        let mut info = sample();
        info.resign(&dir(), &key);
        info.resign(&dir(), &key);
        assert_eq!(info.sigs.len(), 1);
        key.public().verify(&info.fingerprint(&dir()), &info.sigs[0]).expect("verifies");
    }

    #[test]
    fn url_follows_the_file_hash_and_compression() {
        let info = sample();
        assert_eq!(info.nar().url(), format!("nar/{}.nar.zst", info.file_hash.base32()));
    }
}
