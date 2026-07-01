//! Shared entropy primitives for every locally-minted credential.
//!
//! The auto-register flow (install id + registration secret) and the
//! local API (bearer token) all need unpredictable values: an attacker
//! who can guess any of them can impersonate the worker or drive its
//! GPU.  Centralising the OS-CSPRNG access here means there is exactly
//! one audited path instead of per-module copies drifting apart.

use sha2::{Digest, Sha256};

/// Fill `N` bytes from the OS cryptographically-secure RNG via the
/// `getrandom` crate (`getrandom(2)` / `/dev/urandom` on Linux,
/// `getentropy` on macOS, `BCryptGenRandom` on Windows).
///
/// Panics if the OS entropy source is unavailable.  That only happens
/// on a fundamentally broken platform, and failing loudly (the panic
/// is captured by Sentry) is the right call — minting a guessable
/// secret from a timestamp would be worse than a clean crash.
pub fn rand_bytes<const N: usize>() -> [u8; N] {
    let mut buf = [0u8; N];
    getrandom::fill(&mut buf).expect("OS entropy source (getrandom) unavailable");
    buf
}

/// 32 bytes of randomness = 64 hex chars (256 bits of entropy).
pub fn new_secret_hex() -> String {
    let bytes: [u8; 32] = rand_bytes::<32>();
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// UUIDv4-ish without pulling in the `uuid` crate: 16 random bytes
/// formatted as 8-4-4-4-12.
pub fn new_uuid() -> String {
    let bytes: [u8; 16] = rand_bytes::<16>();
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}

/// Hex-encoded SHA-256 of `input`.  Used to send only the *hash* of a
/// locally-held secret over the wire (auto-register).
pub fn sha256_hex(input: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(input.as_bytes());
    let digest = hasher.finalize();
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_uuid_has_expected_shape() {
        let id = new_uuid();
        let parts: Vec<&str> = id.split('-').collect();
        assert_eq!(parts.len(), 5);
        assert_eq!(parts[0].len(), 8);
        assert_eq!(parts[1].len(), 4);
        assert_eq!(parts[2].len(), 4);
        assert_eq!(parts[3].len(), 4);
        assert_eq!(parts[4].len(), 12);
        assert!(id.chars().all(|c| c.is_ascii_hexdigit() || c == '-'));
    }

    #[test]
    fn new_uuid_is_unique() {
        assert_ne!(new_uuid(), new_uuid());
    }

    #[test]
    fn new_secret_hex_is_64_chars() {
        let s = new_secret_hex();
        assert_eq!(s.len(), 64);
        assert!(s.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn sha256_hex_is_deterministic() {
        assert_eq!(sha256_hex("abc"), sha256_hex("abc"));
        assert_ne!(sha256_hex("abc"), sha256_hex("abd"));
        assert_eq!(sha256_hex("").len(), 64);
    }

    // ---------------------------------------------------------------
    // Entropy primitive.  `rand_bytes` is the single source for the
    // install id, registration secret, and local API token on every
    // platform, so these also cover the formerly-untested Windows path
    // (which used to route through a predictable timestamp fallback).
    // ---------------------------------------------------------------

    #[test]
    fn rand_bytes_are_distinct_across_many_calls() {
        use std::collections::HashSet;
        let mut seen = HashSet::new();
        for _ in 0..2_000 {
            assert!(
                seen.insert(rand_bytes::<32>()),
                "rand_bytes produced a duplicate 32-byte value"
            );
        }
    }

    #[test]
    fn rand_bytes_cover_every_bit_position() {
        // OR + AND across many samples: a stuck or constant source
        // would leave a bit position never set (an OR-zero) or never
        // cleared (an AND-one).  An OS CSPRNG flips every one of the
        // 256 bits within a handful of samples.
        let mut ever_set = [0u8; 32];
        let mut ever_clear = [0xffu8; 32];
        for _ in 0..256 {
            let b = rand_bytes::<32>();
            for i in 0..32 {
                ever_set[i] |= b[i];
                ever_clear[i] &= b[i];
            }
        }
        assert_eq!(ever_set, [0xffu8; 32], "a bit position was never set");
        assert_eq!(ever_clear, [0u8; 32], "a bit position was never cleared");
    }
}
