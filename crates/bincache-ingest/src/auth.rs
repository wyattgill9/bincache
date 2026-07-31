//! Per-node bearer tokens for the push path.
//!
//! There is no auth on the read path at all, so this check structurally cannot become a
//! read-path dependency.
//!
//! Tokens are compared as SHA-256 digests rather than as raw strings. That makes every
//! comparison the same fixed width, so neither the token's length nor the position of its
//! first differing byte is observable in the time the check takes.

use subtle::ConstantTimeEq as _;

/// Bytes of entropy in a generated token. 256 bits, so a token is not guessable and does
/// not need a rate limiter behind it to stay safe.
const ENTROPY: usize = 32;

/// The answer to "may this request write?". An enum rather than a bool so a caller cannot
/// read an accidental `true` as permission.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[must_use]
pub enum Admission {
    Admitted,
    Denied,
}

/// The accepted push credentials, fixed at boot. Cheap to clone; shards share one.
#[derive(Clone, Debug)]
pub struct Tokens {
    digests: std::sync::Arc<[[u8; bincache_core::hash::WIDTH]]>,
}

impl Tokens {
    pub fn new(tokens: impl IntoIterator<Item = String>) -> Self {
        let digests: Vec<[u8; bincache_core::hash::WIDTH]> = tokens
            .into_iter()
            .map(|token| *bincache_core::hash::Sha256::digest(token.as_bytes()).as_bytes())
            .collect();
        Self { digests: digests.into() }
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.digests.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.digests.is_empty()
    }

    /// Every configured token is compared, with no early exit, so the number of comparisons
    /// does not depend on which one matched.
    pub fn admits(&self, presented: &str) -> Admission {
        let digest = *bincache_core::hash::Sha256::digest(presented.as_bytes()).as_bytes();
        let mut matched = subtle::Choice::from(0u8);
        for known in self.digests.iter() {
            matched |= digest.ct_eq(known);
        }
        if bool::from(matched) { Admission::Admitted } else { Admission::Denied }
    }
}

/// A fresh credential for one build node, in the `Authorization: Bearer <token>` spelling.
/// Printed once by the operator tool and never stored server-side in this form.
#[must_use]
pub fn generate() -> String {
    let mut entropy = [0u8; ENTROPY];
    rand::Rng::fill(&mut rand::rng(), &mut entropy);
    data_encoding::BASE64URL_NOPAD.encode(&entropy)
}

/// Pulls the credential out of an `Authorization` header value. Case-insensitive on the
/// scheme, per RFC 9110.
#[must_use]
pub fn bearer(header: &str) -> Option<&str> {
    let (scheme, credential) = header.split_once(' ')?;
    if scheme.eq_ignore_ascii_case("Bearer") { Some(credential.trim()) } else { None }
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    #[test]
    fn admits_a_configured_token_and_denies_everything_else() {
        let tokens = crate::auth::Tokens::new(["builder-one".to_owned(), "builder-two".to_owned()]);
        assert_eq!(tokens.admits("builder-one"), crate::auth::Admission::Admitted);
        assert_eq!(tokens.admits("builder-two"), crate::auth::Admission::Admitted);
        assert_eq!(tokens.admits("builder-thr"), crate::auth::Admission::Denied);
        assert_eq!(tokens.admits(""), crate::auth::Admission::Denied);
        assert_eq!(tokens.admits("builder-one "), crate::auth::Admission::Denied);
    }

    #[test]
    fn an_empty_set_admits_nothing() {
        let tokens = crate::auth::Tokens::new([]);
        assert!(tokens.is_empty());
        assert_eq!(tokens.admits("anything"), crate::auth::Admission::Denied);
    }

    #[test]
    fn generated_tokens_are_distinct_and_admitted() {
        let first = crate::auth::generate();
        let second = crate::auth::generate();
        assert_ne!(first, second);

        let tokens = crate::auth::Tokens::new([first.clone()]);
        assert_eq!(tokens.admits(&first), crate::auth::Admission::Admitted);
        assert_eq!(tokens.admits(&second), crate::auth::Admission::Denied);
    }

    #[test]
    fn reads_the_bearer_scheme_case_insensitively() {
        assert_eq!(crate::auth::bearer("Bearer abc"), Some("abc"));
        assert_eq!(crate::auth::bearer("bearer abc"), Some("abc"));
        assert_eq!(crate::auth::bearer("BEARER abc"), Some("abc"));
        assert_eq!(crate::auth::bearer("Basic abc"), None);
        assert_eq!(crate::auth::bearer("abc"), None);
    }
}
