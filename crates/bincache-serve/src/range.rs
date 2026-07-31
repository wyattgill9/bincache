//! `Range` on the payload plane, which is the difference between a resumed NAR download
//! and one that restarts from zero.
//!
//! `maybeRetry` in `nix/src/libstore/filetransfer.cc` resumes a dropped transfer only when
//! the original response advertised `Accept-Ranges: bytes` and carried no
//! `Content-Encoding`. Getting that wrong presents as a mysterious throughput cliff and
//! never as an error, which is why both are asserted by a conformance test.
//!
//! The form actually exercised is the single open-ended `bytes=N-` that
//! `CURLOPT_RESUME_FROM_LARGE` emits. `bytes=N-M` costs nothing extra from the same code,
//! and a multi-range request gets the whole body under a plain `200`, which RFC 9110
//! permits.

/// What the client asked for, resolved against a known artifact length.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Requested {
    /// No usable range header: answer `200` with the whole body.
    Whole,
    /// A satisfiable single range: answer `206`.
    Partial(Span),
    /// The range starts past the end: answer `416` with `Content-Range: bytes *​/<len>`.
    Unsatisfiable,
}

/// A resolved byte span, inclusive of `first` and `last`, as `Content-Range` writes them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Span {
    pub first: u64,
    pub last: u64,
}

impl Span {
    #[must_use]
    pub const fn len(&self) -> u64 {
        self.last - self.first + 1
    }

    #[must_use]
    pub const fn is_empty(&self) -> bool {
        false
    }
}

/// Resolves a header value against the artifact length.
///
/// Anything unparseable resolves to [`Requested::Whole`] rather than an error, matching
/// RFC 9110: a recipient that cannot understand a `Range` must ignore it.
#[must_use]
pub fn resolve(header: Option<&str>, length: u64) -> Requested {
    let Some(header) = header else {
        return Requested::Whole;
    };
    let Some(set) = header.trim().strip_prefix("bytes=") else {
        return Requested::Whole;
    };
    // Multiple ranges are legal to answer with the whole body, and doing so avoids a
    // multipart/byteranges encoder that no Nix client would ever exercise.
    if set.contains(',') {
        return Requested::Whole;
    }
    let Some((first, last)) = set.split_once('-') else {
        return Requested::Whole;
    };

    if first.is_empty() {
        return suffix(last, length);
    }
    let Ok(first) = first.parse() else {
        return Requested::Whole;
    };
    if first >= length {
        return Requested::Unsatisfiable;
    }

    let last = if last.is_empty() {
        length - 1
    } else {
        let Ok(last) = last.parse() else {
            return Requested::Whole;
        };
        // A last-byte-pos past the end is clamped, not refused.
        core::cmp::min(last, length - 1)
    };

    if last < first { Requested::Unsatisfiable } else { Requested::Partial(Span { first, last }) }
}

/// `bytes=-N`: the final `N` bytes. Not something nix sends, but cheap to answer correctly
/// from the same code rather than getting it subtly wrong.
fn suffix(last: &str, length: u64) -> Requested {
    let Ok(wanted) = last.parse() else {
        return Requested::Whole;
    };
    if wanted == 0 {
        return Requested::Unsatisfiable;
    }
    let first = length.saturating_sub(wanted);
    Requested::Partial(Span { first, last: length - 1 })
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    /// The only form `CURLOPT_RESUME_FROM_LARGE` emits.
    #[test]
    fn resolves_the_resume_form_nix_sends() {
        assert_eq!(
            crate::range::resolve(Some("bytes=100-"), 1000),
            crate::range::Requested::Partial(crate::range::Span { first: 100, last: 999 })
        );
    }

    #[test]
    fn resolves_a_closed_range() {
        assert_eq!(
            crate::range::resolve(Some("bytes=10-19"), 1000),
            crate::range::Requested::Partial(crate::range::Span { first: 10, last: 19 })
        );
    }

    #[test]
    fn clamps_a_last_byte_past_the_end() {
        assert_eq!(
            crate::range::resolve(Some("bytes=990-5000"), 1000),
            crate::range::Requested::Partial(crate::range::Span { first: 990, last: 999 })
        );
    }

    #[test]
    fn resolves_a_suffix_range() {
        assert_eq!(
            crate::range::resolve(Some("bytes=-10"), 1000),
            crate::range::Requested::Partial(crate::range::Span { first: 990, last: 999 })
        );
        assert_eq!(
            crate::range::resolve(Some("bytes=-5000"), 1000),
            crate::range::Requested::Partial(crate::range::Span { first: 0, last: 999 })
        );
    }

    #[test]
    fn refuses_a_range_that_starts_past_the_end() {
        assert_eq!(
            crate::range::resolve(Some("bytes=1000-"), 1000),
            crate::range::Requested::Unsatisfiable
        );
        assert_eq!(
            crate::range::resolve(Some("bytes=-0"), 1000),
            crate::range::Requested::Unsatisfiable
        );
        assert_eq!(
            crate::range::resolve(Some("bytes=50-10"), 1000),
            crate::range::Requested::Unsatisfiable
        );
    }

    #[test]
    fn ignores_what_it_cannot_understand() {
        for header in ["items=1-2", "bytes=abc-", "bytes=1-def", "bytes=0-1,5-6", "nonsense"] {
            assert_eq!(
                crate::range::resolve(Some(header), 1000),
                crate::range::Requested::Whole,
                "{header:?} should have been ignored"
            );
        }
        assert_eq!(crate::range::resolve(None, 1000), crate::range::Requested::Whole);
    }

    #[test]
    fn a_span_length_counts_both_ends() {
        assert_eq!(crate::range::Span { first: 0, last: 0 }.len(), 1);
        assert_eq!(crate::range::Span { first: 100, last: 999 }.len(), 900);
    }
}
