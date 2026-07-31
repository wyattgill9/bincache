//! Nix's base32 alphabet and the reversed bit layout `nix/src/libutil/hash.cc` uses.
//!
//! The layout is not RFC 4648. Nix emits the *last* character first and packs bits
//! little-endian across bytes, so a straight base32 codec produces a different string for
//! the same input. Both directions here mirror `printHash32` / `parseHash32`.

/// Base32 minus `e`, `o`, `u`, `t`, so generated store paths cannot contain accidental
/// words. 160 bits at 5 bits per character is exactly 32 characters, with no padding.
const ALPHABET: [u8; 32] = *b"0123456789abcdfghijklmnpqrsvwxyz";

/// Sentinel for a lowercase letter that `ALPHABET` drops. Out of range of any real digit.
const DROPPED: u8 = u8::MAX;

/// `ALPHABET` position of each ASCII lowercase letter, `DROPPED` for `e`, `o`, `t`, `u`.
const LETTERS: [u8; 26] = [
    10, 11, 12, 13, DROPPED, 14, 15, 16, 17, 18, 19, 20, 21, 22, DROPPED, 23, 24, 25, 26, DROPPED,
    DROPPED, 27, 28, 29, 30, 31,
];

/// Bits carried by one base32 character.
const BITS: usize = 5;

#[derive(Debug, snafu::Snafu)]
#[snafu(visibility(pub))]
pub enum Error {
    #[snafu(display("expected {expected} base32 characters, found {found}"))]
    Length { expected: usize, found: usize },

    #[snafu(display("byte {byte:#04x} is not in the nix base32 alphabet"))]
    Alphabet { byte: u8 },

    #[snafu(display("base32 text sets {overflow} bits past the end of a {width}-byte value"))]
    Overflow { overflow: u8, width: usize },
}

/// Characters needed to encode `width` bytes. Ceiling division, since the final character
/// may carry fewer than five significant bits.
#[must_use]
pub const fn text_len(width: usize) -> usize {
    (width * 8 - 1) / BITS + 1
}

#[must_use]
pub fn encode(bytes: &[u8]) -> String {
    let mut text = String::with_capacity(text_len(bytes.len()));
    for index in (0..text_len(bytes.len())).rev() {
        let bit = index * BITS;
        let byte = bit / 8;
        let shift = bit % 8;
        let low = bytes[byte];
        let high = if byte + 1 < bytes.len() { bytes[byte + 1] } else { 0 };
        let window = u16::from_le_bytes([low, high]) >> shift;
        text.push(char::from(ALPHABET[usize::from(window & 0x1f)]));
    }
    text
}

/// Decodes exactly `N` bytes, rejecting any text whose final character sets bits the value
/// cannot hold. `N` comes from the binding, so no turbofish is needed at call sites.
pub fn decode<const N: usize>(text: &str) -> Result<[u8; N], Error> {
    let expected = text_len(N);
    snafu::ensure!(text.len() == expected, LengthSnafu { expected, found: text.len() });

    let mut bytes = [0u8; N];
    for (index, character) in text.bytes().rev().enumerate() {
        let digit = digit(character)?;
        let bit = index * BITS;
        let byte = bit / 8;
        let shift = bit % 8;
        let [low, high] = (u16::from(digit) << shift).to_le_bytes();
        bytes[byte] |= low;
        if byte + 1 < N {
            bytes[byte + 1] |= high;
        } else {
            snafu::ensure!(high == 0, OverflowSnafu { overflow: high, width: N });
        }
    }
    Ok(bytes)
}

fn digit(character: u8) -> Result<u8, Error> {
    match character {
        b'0'..=b'9' => Ok(character - b'0'),
        b'a'..=b'z' => {
            let position = LETTERS[usize::from(character - b'a')];
            snafu::ensure!(position != DROPPED, AlphabetSnafu { byte: character });
            Ok(position)
        }
        byte => AlphabetSnafu { byte }.fail(),
    }
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    /// `nix hash to-base32 --type sha256` on the empty string.
    const EMPTY_SHA256: [u8; 32] = [
        0xe3, 0xb0, 0xc4, 0x42, 0x98, 0xfc, 0x1c, 0x14, 0x9a, 0xfb, 0xf4, 0xc8, 0x99, 0x6f, 0xb9,
        0x24, 0x27, 0xae, 0x41, 0xe4, 0x64, 0x9b, 0x93, 0x4c, 0xa4, 0x95, 0x99, 0x1b, 0x78, 0x52,
        0xb8, 0x55,
    ];

    #[test]
    fn text_len_matches_nix() {
        assert_eq!(crate::base32::text_len(20), 32);
        assert_eq!(crate::base32::text_len(32), 52);
    }

    #[test]
    fn encodes_the_empty_sha256_like_nix() {
        assert_eq!(
            crate::base32::encode(&EMPTY_SHA256),
            "0mdqa9w1p6cmli6976v4wi0sw9r4p5prkj7lzfd1877wk11c9c73"
        );
    }

    #[test]
    fn decodes_what_it_encodes() {
        let text = crate::base32::encode(&EMPTY_SHA256);
        let bytes: [u8; 32] = crate::base32::decode(&text).expect("round trip");
        assert_eq!(bytes, EMPTY_SHA256);
    }

    /// Every 20-byte value drawn from a fixed LCG survives encode then decode. Property in
    /// shape, deterministic in execution, so a failure is reproducible from the seed.
    #[test]
    fn round_trips_every_sampled_store_path_hash() {
        let mut state: u64 = 0x2545_f491_4f6c_dd1d;
        for _ in 0..4096 {
            let mut bytes = [0u8; 20];
            for slot in &mut bytes {
                state = state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                *slot = state.to_le_bytes()[7];
            }
            let text = crate::base32::encode(&bytes);
            assert_eq!(text.len(), 32);
            let decoded: [u8; 20] = crate::base32::decode(&text).expect("round trip");
            assert_eq!(decoded, bytes);
        }
    }

    #[test]
    fn rejects_dropped_letters() {
        let text = "e".repeat(32);
        let decoded: Result<[u8; 20], _> = crate::base32::decode(&text);
        assert!(matches!(decoded, Err(crate::base32::Error::Alphabet { byte: b'e' })));
    }

    #[test]
    fn rejects_wrong_length() {
        let decoded: Result<[u8; 20], _> = crate::base32::decode("00");
        assert!(matches!(decoded, Err(crate::base32::Error::Length { expected: 32, found: 2 })));
    }

    /// 52 characters carry 260 bits; a 32-byte value holds 256. The leading character may
    /// therefore only be one of the first sixteen alphabet entries.
    #[test]
    fn rejects_overflowing_padding_bits() {
        let mut text = crate::base32::encode(&EMPTY_SHA256);
        text.replace_range(0..1, "z");
        let decoded: Result<[u8; 32], _> = crate::base32::decode(&text);
        assert!(matches!(decoded, Err(crate::base32::Error::Overflow { .. })));
    }
}
