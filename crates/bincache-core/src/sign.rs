//! ed25519 over the narinfo fingerprint, in the key and signature spellings Nix uses.
//!
//! A client accepts a path if *any* `Sig` line matches a key in its `trusted-public-keys`,
//! which is what makes rotation non-disruptive: publish the new public key first, then
//! switch the signing key.

use ed25519_dalek::Signer as _;
use ed25519_dalek::Verifier as _;
use snafu::OptionExt as _;
use snafu::ResultExt as _;

/// Bytes in an ed25519 signature.
pub const SIGNATURE_WIDTH: usize = 64;

/// Bytes in an ed25519 public key, and in the seed half of a secret key file.
pub const KEY_WIDTH: usize = 32;

/// One `Sig:` line: `<key-name>:<base64 ed25519 signature>`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Signature {
    name: String,
    bytes: [u8; SIGNATURE_WIDTH],
}

/// The signing key. It lives only on the server, which is what bounds a compromised build
/// node to poisoning what it uploads rather than forging signatures for arbitrary paths.
#[derive(Clone)]
pub struct SecretKey {
    name: String,
    key: ed25519_dalek::SigningKey,
}

#[derive(Clone)]
pub struct PublicKey {
    name: String,
    key: ed25519_dalek::VerifyingKey,
}

#[derive(Debug, snafu::Snafu)]
#[snafu(visibility(pub))]
pub enum Error {
    #[snafu(display("key material {text:?} has no `<name>:<base64>` separator"))]
    Separator { text: String },

    #[snafu(display("key name is empty"))]
    NameEmpty,

    #[snafu(display("base64 payload is malformed"))]
    Base64 { source: data_encoding::DecodeError },

    #[snafu(display("payload is not the expected byte width"))]
    Width { source: core::array::TryFromSliceError },

    #[snafu(display("public key bytes are not a valid ed25519 point"))]
    Point { source: ed25519_dalek::SignatureError },

    #[snafu(display("no signature from key {name:?} is present"))]
    KeyMismatch { name: String },

    #[snafu(display("signature does not verify under key {name:?}"))]
    Invalid { name: String, source: ed25519_dalek::SignatureError },
}

/// Splits the `<name>:<base64>` shape both key files and `Sig` lines use.
fn split(text: &str) -> Result<(String, Vec<u8>), Error> {
    let (name, encoded) = text.split_once(':').context(SeparatorSnafu { text })?;
    snafu::ensure!(!name.is_empty(), NameEmptySnafu);
    let bytes = data_encoding::BASE64.decode(encoded.as_bytes()).context(Base64Snafu)?;
    Ok((name.to_owned(), bytes))
}

fn render(name: &str, bytes: &[u8]) -> String {
    format!("{name}:{}", data_encoding::BASE64.encode(bytes))
}

impl Signature {
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn parse(text: &str) -> Result<Self, Error> {
        let (name, bytes) = split(text)?;
        let sized: [u8; SIGNATURE_WIDTH] = bytes.as_slice().try_into().context(WidthSnafu)?;
        Ok(Self { name, bytes: sized })
    }
}

impl core::fmt::Display for Signature {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter.write_str(&render(&self.name, &self.bytes))
    }
}

impl SecretKey {
    /// Parses a `nix-store --generate-binary-cache-key` secret key file: the name, then
    /// base64 of the 32-byte seed followed by the 32-byte public key.
    pub fn parse(text: &str) -> Result<Self, Error> {
        let (name, bytes) = split(text.trim())?;
        let pair: [u8; KEY_WIDTH * 2] = bytes.as_slice().try_into().context(WidthSnafu)?;
        let (seed, _public) = pair.split_at(KEY_WIDTH);
        let seed: [u8; KEY_WIDTH] = seed.try_into().context(WidthSnafu)?;
        Ok(Self { name, key: ed25519_dalek::SigningKey::from_bytes(&seed) })
    }

    /// Draws a fresh seed rather than calling `SigningKey::generate`, so the RNG stays the
    /// workspace's `rand` rather than whichever `rand_core` major `ed25519-dalek` tracks.
    #[must_use]
    pub fn generate(name: String) -> Self {
        let mut seed = [0u8; KEY_WIDTH];
        rand::Rng::fill(&mut rand::rng(), &mut seed);
        Self { name, key: ed25519_dalek::SigningKey::from_bytes(&seed) }
    }

    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    #[must_use]
    pub fn public(&self) -> PublicKey {
        PublicKey { name: self.name.clone(), key: self.key.verifying_key() }
    }

    /// The secret key file spelling, for `bincache keygen`. Never logged.
    #[must_use]
    pub fn render(&self) -> String {
        let mut pair = [0u8; KEY_WIDTH * 2];
        pair[..KEY_WIDTH].copy_from_slice(&self.key.to_bytes());
        pair[KEY_WIDTH..].copy_from_slice(self.key.verifying_key().as_bytes());
        render(&self.name, &pair)
    }

    #[must_use]
    pub fn sign(&self, fingerprint: &str) -> Signature {
        Signature {
            name: self.name.clone(),
            bytes: self.key.sign(fingerprint.as_bytes()).to_bytes(),
        }
    }
}

impl PublicKey {
    pub fn parse(text: &str) -> Result<Self, Error> {
        let (name, bytes) = split(text.trim())?;
        let sized: [u8; KEY_WIDTH] = bytes.as_slice().try_into().context(WidthSnafu)?;
        let key = ed25519_dalek::VerifyingKey::from_bytes(&sized).context(PointSnafu)?;
        Ok(Self { name, key })
    }

    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The `trusted-public-keys` spelling clients need in order to accept what we sign.
    #[must_use]
    pub fn render(&self) -> String {
        render(&self.name, self.key.as_bytes())
    }

    pub fn verify(&self, fingerprint: &str, signature: &Signature) -> Result<(), Error> {
        snafu::ensure!(signature.name == self.name, KeyMismatchSnafu { name: self.name.clone() });
        let parsed = ed25519_dalek::Signature::from_bytes(&signature.bytes);
        self.key
            .verify(fingerprint.as_bytes(), &parsed)
            .with_context(|_| InvalidSnafu { name: self.name.clone() })
    }
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    const FINGERPRINT: &str = "1;/nix/store/5rnvz1n7hdmvbdzq0d5m5xrz3xz6ky8j-hello;sha256:x;1;";

    #[test]
    fn signs_what_its_public_half_verifies() {
        let secret = crate::sign::SecretKey::generate("bincache-test-1".to_owned());
        let signature = secret.sign(FINGERPRINT);
        assert_eq!(signature.name(), "bincache-test-1");
        secret.public().verify(FINGERPRINT, &signature).expect("verifies");
    }

    #[test]
    fn rejects_a_signature_over_different_bytes() {
        let secret = crate::sign::SecretKey::generate("bincache-test-1".to_owned());
        let signature = secret.sign(FINGERPRINT);
        let other = secret.public().verify("1;other;sha256:x;1;", &signature);
        assert!(matches!(other, Err(crate::sign::Error::Invalid { .. })));
    }

    #[test]
    fn round_trips_key_and_signature_text() {
        let secret = crate::sign::SecretKey::generate("bincache-test-1".to_owned());
        let reparsed = crate::sign::SecretKey::parse(&secret.render()).expect("parses");
        assert_eq!(reparsed.public().render(), secret.public().render());

        let signature = secret.sign(FINGERPRINT);
        let text = signature.to_string();
        assert_eq!(crate::sign::Signature::parse(&text).expect("parses"), signature);

        let public = crate::sign::PublicKey::parse(&secret.public().render()).expect("parses");
        public.verify(FINGERPRINT, &signature).expect("verifies");
    }

    #[test]
    fn rejects_a_signature_from_an_unknown_key() {
        let secret = crate::sign::SecretKey::generate("bincache-test-1".to_owned());
        let stranger = crate::sign::SecretKey::generate("someone-else-1".to_owned());
        let signature = stranger.sign(FINGERPRINT);
        assert!(matches!(
            secret.public().verify(FINGERPRINT, &signature),
            Err(crate::sign::Error::KeyMismatch { .. })
        ));
    }
}
