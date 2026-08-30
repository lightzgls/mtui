//! The `SAPISIDHASH` scheme Google's own web clients authenticate with.
//!
//! Google's web players sign each private request with a hash over a cookie,
//! which is what this file computes.
//!
//! The scheme is unauthenticated in the cryptographic sense: it proves only
//! that the caller knows `SAPISID`, which is a cookie the user copied out of
//! their own browser. Nothing here is a secret this program chose or can
//! rotate, and nothing is sent anywhere except to the origin it is signed for.
//!
//! SHA-1 is hand-rolled rather than pulled in. `sha1` costs five transitive
//! crates (`digest`, `block-buffer`, `crypto-common`, `generic-array`,
//! `typenum`) for one function, and this is the same trade the ISO-8601 parser
//! elsewhere in the source layer already makes. It is checked against the
//! published vectors below.

use std::time::{SystemTime, UNIX_EPOCH};

/// Builds the `Authorization` header value for a request to `origin`.
///
/// The timestamp is part of the signed input and is sent alongside the hash, so
/// Google can bound how long a signature stays good. It is passed in rather
/// than read from the clock here, which is what makes this testable against a
/// fixed expected value.
pub fn authorization(sapisid: &str, origin: &str, now: u64) -> String {
    let digest = sha1_hex(format!("{now} {sapisid} {origin}").as_bytes());
    format!("SAPISIDHASH {now}_{digest}")
}

/// Seconds since the epoch, for signing a request now.
pub fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|since| since.as_secs())
        .unwrap_or(0)
}

/// SHA-1 of `data`, lowercase hex. RFC 3174.
#[allow(unknown_lints)]
#[allow(clippy::chunks_exact_to_as_chunks)]
fn sha1_hex(data: &[u8]) -> String {
    let mut state: [u32; 5] = [
        0x6745_2301,
        0xEFCD_AB89,
        0x98BA_DCFE,
        0x1032_5476,
        0xC3D2_E1F0,
    ];

    // Pad to a whole number of 64-byte blocks: a 1 bit, zeroes, then the
    // original length in bits as a big-endian 64-bit integer.
    let bits = (data.len() as u64).wrapping_mul(8);
    let mut message = data.to_vec();
    message.push(0x80);
    while message.len() % 64 != 56 {
        message.push(0);
    }
    message.extend_from_slice(&bits.to_be_bytes());

    for block in message.chunks_exact(64) {
        let mut w = [0u32; 80];
        for (i, word) in block.chunks_exact(4).enumerate() {
            w[i] = u32::from_be_bytes([word[0], word[1], word[2], word[3]]);
        }
        for i in 16..80 {
            w[i] = (w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16]).rotate_left(1);
        }

        let [mut a, mut b, mut c, mut d, mut e] = state;
        for (round, word) in w.iter().enumerate() {
            let (mix, constant) = match round {
                0..=19 => ((b & c) | (!b & d), 0x5A82_7999),
                20..=39 => (b ^ c ^ d, 0x6ED9_EBA1),
                40..=59 => ((b & c) | (b & d) | (c & d), 0x8F1B_BCDC),
                _ => (b ^ c ^ d, 0xCA62_C1D6),
            };
            let next = a
                .rotate_left(5)
                .wrapping_add(mix)
                .wrapping_add(e)
                .wrapping_add(constant)
                .wrapping_add(*word);
            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = next;
        }

        for (word, added) in state.iter_mut().zip([a, b, c, d, e]) {
            *word = word.wrapping_add(added);
        }
    }

    state.iter().map(|word| format!("{word:08x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The published vectors. A hash that is subtly wrong would present as
    /// "YouTube refuses the cookie", which is indistinguishable from an expired
    /// one -- so this is the test that keeps a signing bug from being blamed on
    /// the user's browser.
    #[test]
    fn matches_the_rfc_3174_vectors() {
        assert_eq!(sha1_hex(b"abc"), "a9993e364706816aba3e25717850c26c9cd0d89d");
        assert_eq!(
            sha1_hex(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"),
            "84983e441c3bd26ebaae4aa1f95129e5e54670f1"
        );
        assert_eq!(sha1_hex(b""), "da39a3ee5e6b4b0d3255bfef95601890afd80709");
    }

    /// A block boundary either side: the padding rule is where a hand-rolled
    /// SHA-1 goes wrong, and it goes wrong only at particular lengths.
    #[test]
    fn pads_correctly_around_the_block_boundary() {
        // 55, 56 and 64 bytes: the last message that fits its length in the
        // same block, the first that does not, and an exact block.
        assert_eq!(
            sha1_hex(&[b'a'; 55]),
            "c1c8bbdc22796e28c0e15163d20899b65621d65a"
        );
        assert_eq!(
            sha1_hex(&[b'a'; 56]),
            "c2db330f6083854c99d4b5bfb6e8f29f201be699"
        );
        assert_eq!(
            sha1_hex(&[b'a'; 64]),
            "0098ba824b5c16427bd7a1122a5a442a25ec644d"
        );
    }

    /// The exact shape Google's clients send, so a change here is visible
    /// rather than something only YouTube notices.
    #[test]
    fn signs_in_the_shape_google_expects() {
        let header = authorization("SECRET", "https://music.youtube.com", 1_700_000_000);
        let (scheme, rest) = header.split_once(' ').expect("scheme and value");
        assert_eq!(scheme, "SAPISIDHASH");

        let (stamp, digest) = rest.split_once('_').expect("timestamp and digest");
        assert_eq!(stamp, "1700000000");
        assert_eq!(digest.len(), 40, "a SHA-1 is 40 hex characters");
        assert!(digest.chars().all(|c| c.is_ascii_hexdigit()));

        // Signed over "timestamp SAPISID origin", in that order and space
        // separated -- get the order wrong and every request is refused.
        assert_eq!(
            digest,
            sha1_hex(b"1700000000 SECRET https://music.youtube.com")
        );
    }

    #[test]
    fn a_different_moment_signs_differently() {
        let origin = "https://music.youtube.com";
        assert_ne!(
            authorization("SECRET", origin, 1_700_000_000),
            authorization("SECRET", origin, 1_700_000_001)
        );
    }
}
