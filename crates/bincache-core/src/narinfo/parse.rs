//! Reading a narinfo body back into a record.
//!
//! Needed on the write path: `nix copy --to` uploads the NAR and then `PUT`s the narinfo
//! text it rendered locally. It is also the other half of the render round-trip property.
//!
//! Leniency matches `NarInfo::NarInfo` in `nix/src/libstore/nar-info.cc`: unknown keys are
//! ignored, and an absent `Compression` means bzip2 rather than none.

use snafu::OptionExt as _;
use snafu::ResultExt as _;

/// The keys a narinfo body may carry. Spellings are the wire spellings, so no call site
/// writes a bare string for one.
#[derive(Clone, Copy, Debug, PartialEq, Eq, strum::Display, strum::EnumString)]
pub enum Field {
    StorePath,
    #[strum(serialize = "URL")]
    Url,
    Compression,
    FileHash,
    FileSize,
    NarHash,
    NarSize,
    References,
    Deriver,
    Sig,
    #[strum(serialize = "CA")]
    Ca,
}

/// `nix/src/libstore/nar-info.cc` writes this in place of a `Deriver:` path it does not
/// know, and skips the field on read when it sees it.
const DERIVER_UNKNOWN: &str = "unknown-deriver";

#[derive(Debug, snafu::Snafu)]
#[snafu(visibility(pub))]
pub enum Error {
    #[snafu(display("narinfo line {line:?} is not `Key: Value`"))]
    Line { line: String },

    #[snafu(display("narinfo is missing the required {field} field"))]
    Missing { field: Field },

    #[snafu(display("narinfo {field} is not a store path"))]
    Path { field: Field, source: crate::storepath::Error },

    #[snafu(display("narinfo {field} is not a sha256 digest"))]
    Digest { field: Field, source: crate::hash::Error },

    #[snafu(display("narinfo {field} is not an integer"))]
    Number { field: Field, source: core::num::ParseIntError },

    #[snafu(display("narinfo NarSize is zero, which nix rejects as corrupt"))]
    NarSizeZero,

    #[snafu(display("narinfo Compression names an algorithm this build does not know"))]
    Algorithm { source: strum::ParseError },

    #[snafu(display("narinfo Sig line is malformed"))]
    Signature { source: crate::sign::Error },
}

/// Fields accumulated before the required-field check runs, so a body missing `NarHash`
/// reports that rather than failing on whichever line came first.
#[derive(Default)]
struct Draft {
    store_path: Option<crate::storepath::Path>,
    url: Option<String>,
    compression: Option<crate::compression::Compression>,
    file_hash: Option<crate::hash::Sha256>,
    file_size: Option<u64>,
    nar_hash: Option<crate::hash::Sha256>,
    nar_size: Option<u64>,
    references: Vec<crate::storepath::Path>,
    deriver: Option<crate::storepath::Path>,
    sigs: Vec<crate::sign::Signature>,
    ca: Option<String>,
}

pub fn parse(body: &str, dir: &crate::storepath::Dir) -> Result<crate::narinfo::NarInfo, Error> {
    let mut draft = Draft::default();
    for line in body.lines().filter(|line| !line.is_empty()) {
        // `Key: value` is what nix writes. A bare `Key:` is accepted as an empty value so a
        // narinfo from another implementation, which may have dropped the trailing space on
        // an empty `References`, still reads rather than failing the whole body.
        let (key, value) = match line.split_once(": ") {
            Some(split) => split,
            None => (line.strip_suffix(':').context(LineSnafu { line })?, ""),
        };
        let Ok(field) = core::str::FromStr::from_str(key) else {
            continue;
        };
        absorb(&mut draft, field, value, dir)?;
    }
    finish(draft)
}

fn absorb(
    draft: &mut Draft,
    field: Field,
    value: &str,
    dir: &crate::storepath::Dir,
) -> Result<(), Error> {
    match field {
        Field::StorePath => {
            draft.store_path = Some(dir.parse(value).context(PathSnafu { field })?);
        }
        Field::Url => draft.url = Some(value.to_owned()),
        Field::Compression => {
            draft.compression = Some(core::str::FromStr::from_str(value).context(AlgorithmSnafu)?);
        }
        Field::FileHash => {
            draft.file_hash =
                Some(crate::hash::Sha256::parse(value).context(DigestSnafu { field })?);
        }
        Field::FileSize => {
            draft.file_size = Some(value.parse().context(NumberSnafu { field })?);
        }
        Field::NarHash => {
            draft.nar_hash =
                Some(crate::hash::Sha256::parse(value).context(DigestSnafu { field })?);
        }
        Field::NarSize => {
            draft.nar_size = Some(value.parse().context(NumberSnafu { field })?);
        }
        Field::References => {
            for name in value.split(' ').filter(|name| !name.is_empty()) {
                let reference = crate::storepath::Path::parse(name).context(PathSnafu { field })?;
                draft.references.push(reference);
            }
        }
        Field::Deriver => {
            if value != DERIVER_UNKNOWN {
                let parsed = crate::storepath::Path::parse(value).context(PathSnafu { field })?;
                draft.deriver = Some(parsed);
            }
        }
        Field::Sig => {
            draft.sigs.push(crate::sign::Signature::parse(value).context(SignatureSnafu)?);
        }
        Field::Ca => draft.ca = Some(value.to_owned()),
    }
    Ok(())
}

/// The conformance floor: `NarInfo::NarInfo` throws `corrupt` unless `StorePath`,
/// `NarHash`, and `URL` are present and `NarSize` is nonzero. `FileHash` and `FileSize` are
/// required on top of that because bincache's record describes the artifact it serves, and
/// every binary-cache upload nix produces carries both.
fn finish(draft: Draft) -> Result<crate::narinfo::NarInfo, Error> {
    let Draft {
        store_path,
        url,
        compression,
        file_hash,
        file_size,
        nar_hash,
        nar_size,
        references,
        deriver,
        sigs,
        ca,
    } = draft;

    let nar_size = nar_size.context(MissingSnafu { field: Field::NarSize })?;
    url.filter(|url| !url.is_empty()).context(MissingSnafu { field: Field::Url })?;

    Ok(crate::narinfo::NarInfo {
        store_path: store_path.context(MissingSnafu { field: Field::StorePath })?,
        // An absent field means bzip2, not none: `nar-info.cc` defaults it that way.
        compression: compression.unwrap_or(crate::compression::Compression::Bzip2),
        file_hash: file_hash.context(MissingSnafu { field: Field::FileHash })?,
        file_size: file_size.context(MissingSnafu { field: Field::FileSize })?,
        nar_hash: nar_hash.context(MissingSnafu { field: Field::NarHash })?,
        nar_size: core::num::NonZeroU64::new(nar_size).context(NarSizeZeroSnafu)?,
        references,
        deriver,
        sigs,
        ca,
    })
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    fn dir() -> crate::storepath::Dir {
        crate::storepath::Dir::new(crate::storepath::DIR_DEFAULT.to_owned()).expect("absolute")
    }

    #[test]
    fn round_trips_a_rendered_record() {
        let mut original = crate::narinfo::tests::sample();
        let key = crate::sign::SecretKey::generate("bincache-test-1".to_owned());
        original.resign(&dir(), &key);

        let body = original.render(&dir());
        let parsed = crate::narinfo::parse::parse(&body, &dir()).expect("parses");
        assert_eq!(parsed, original);
        assert_eq!(parsed.render(&dir()), body);
    }

    #[test]
    fn ignores_unknown_keys() {
        let body = crate::narinfo::tests::sample().render(&dir());
        let extended = format!("{body}Unknown: whatever\n");
        let parsed = crate::narinfo::parse::parse(&extended, &dir()).expect("parses");
        assert_eq!(parsed, crate::narinfo::tests::sample());
    }

    #[test]
    fn treats_an_absent_compression_field_as_bzip2() {
        let body = crate::narinfo::tests::sample().render(&dir());
        let stripped: String = body.lines().filter(|line| !line.starts_with("Compression:")).fold(
            String::new(),
            |mut text, line| {
                text.push_str(line);
                text.push('\n');
                text
            },
        );
        let parsed = crate::narinfo::parse::parse(&stripped, &dir()).expect("parses");
        assert_eq!(parsed.compression, crate::compression::Compression::Bzip2);
    }

    #[test]
    fn drops_an_unknown_deriver() {
        let body =
            format!("{}Deriver: unknown-deriver\n", crate::narinfo::tests::sample().render(&dir()));
        let parsed = crate::narinfo::parse::parse(&body, &dir()).expect("parses");
        assert_eq!(parsed.deriver, crate::narinfo::tests::sample().deriver);
    }

    /// The conformance floor from `research/DESIGN_V2.md`: a body missing any of these is
    /// caught here rather than by a client's `corrupt` error.
    #[test]
    fn rejects_each_missing_required_field() {
        let body = crate::narinfo::tests::sample().render(&dir());
        for (prefix, field) in [
            ("StorePath:", crate::narinfo::parse::Field::StorePath),
            ("URL:", crate::narinfo::parse::Field::Url),
            ("NarHash:", crate::narinfo::parse::Field::NarHash),
            ("NarSize:", crate::narinfo::parse::Field::NarSize),
        ] {
            let stripped: String = body
                .lines()
                .filter(|line| !line.starts_with(prefix))
                .map(|line| format!("{line}\n"))
                .collect();
            let parsed = crate::narinfo::parse::parse(&stripped, &dir());
            assert!(
                matches!(parsed, Err(crate::narinfo::parse::Error::Missing { field: got })
                    if got == field),
                "{prefix} removal should report a missing {field}"
            );
        }
    }

    #[test]
    fn rejects_a_zero_nar_size() {
        let body =
            crate::narinfo::tests::sample().render(&dir()).replace("NarSize: 226504", "NarSize: 0");
        assert!(matches!(
            crate::narinfo::parse::parse(&body, &dir()),
            Err(crate::narinfo::parse::Error::NarSizeZero)
        ));
    }

    /// Another implementation may drop the trailing space on an empty `References`. Reading
    /// that is harmless; writing it is not, which is why only the parser is lenient.
    #[test]
    fn accepts_a_bare_key_as_an_empty_value() {
        let body = crate::narinfo::tests::sample()
            .render(&dir())
            .replace("References: 5rnvz", "References:\nIgnored: 5rnvz");
        let parsed = crate::narinfo::parse::parse(&body, &dir()).expect("parses");
        assert!(parsed.references.is_empty());
    }

    #[test]
    fn rejects_a_line_without_a_separator() {
        let body = format!("{}garbage\n", crate::narinfo::tests::sample().render(&dir()));
        assert!(matches!(
            crate::narinfo::parse::parse(&body, &dir()),
            Err(crate::narinfo::parse::Error::Line { .. })
        ));
    }
}
