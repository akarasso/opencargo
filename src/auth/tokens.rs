use rand::Rng;
use sha2::{Digest, Sha256};

/// Generate a new API token with the given prefix.
///
/// Returns `(raw_token, token_hash)`.
/// Token format: `{prefix}{random_32_hex}` (e.g., `trg_a1b2c3d4...`).
pub fn generate_token(prefix: &str) -> (String, String) {
    let mut rng = rand::thread_rng();
    let random_bytes: [u8; 16] = rng.gen();
    let hex_part: String = random_bytes.iter().map(|b| format!("{b:02x}")).collect();
    let raw_token = format!("{prefix}{hex_part}");
    let hash = hash_token(&raw_token);
    (raw_token, hash)
}

/// Compute a SHA-256 hex hash of the given token.
pub fn hash_token(token: &str) -> String {
    let digest = Sha256::digest(token.as_bytes());
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

/// Verify that a raw token matches a stored hash using constant-time comparison.
pub fn verify_token(token: &str, hash: &str) -> bool {
    let computed = hash_token(token);
    if computed.len() != hash.len() {
        return false;
    }
    // Constant-time comparison to prevent timing attacks
    computed
        .bytes()
        .zip(hash.bytes())
        .fold(0u8, |acc, (a, b)| acc | (a ^ b))
        == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_token_has_prefix_shape_and_verifies_against_its_hash() {
        let (raw, hash) = generate_token("trg_");
        assert!(raw.starts_with("trg_"));
        assert_eq!(raw.len(), "trg_".len() + 32, "16 random bytes → 32 hex chars");
        assert!(raw["trg_".len()..].bytes().all(|b| b.is_ascii_hexdigit()));
        assert_eq!(hash.len(), 64, "SHA-256 → 64 hex chars");
        assert!(hash.bytes().all(|b| b.is_ascii_hexdigit()));
        assert_eq!(hash, hash_token(&raw), "returned hash is the hash of the raw token");
        assert!(verify_token(&raw, &hash));
    }

    #[test]
    fn two_generated_tokens_differ() {
        let (raw1, hash1) = generate_token("trg_");
        let (raw2, hash2) = generate_token("trg_");
        assert_ne!(raw1, raw2);
        assert_ne!(hash1, hash2);
    }

    /// Pins the algorithm (SHA-256) and the encoding (lowercase hex) so a
    /// refactor cannot silently invalidate every token hash stored in the DB.
    #[test]
    fn hash_token_matches_known_sha256_vector() {
        assert_eq!(
            hash_token("test"),
            "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08"
        );
    }

    #[test]
    fn wrong_token_is_rejected() {
        let (_raw, hash) = generate_token("trg_");
        let (other_raw, _) = generate_token("trg_");
        assert!(!verify_token(&other_raw, &hash));
        assert!(!verify_token("trg_deadbeef", &hash));
    }

    #[test]
    fn length_mismatch_is_rejected_without_panic() {
        let (raw, hash) = generate_token("trg_");
        assert!(!verify_token(&raw, ""));
        assert!(!verify_token(&raw, "abc"));
        assert!(!verify_token(&raw, &hash[..63]));
        assert!(!verify_token(&raw, &format!("{hash}00")));
        // Empty token still hashes to 64 chars — same length, wrong value.
        assert!(!verify_token("", &hash));
    }

    #[test]
    fn single_character_flip_is_rejected() {
        let (raw, hash) = generate_token("trg_");

        // Flip the last character of the stored hash.
        let mut bad_hash = hash.clone().into_bytes();
        let last = bad_hash.last_mut().unwrap();
        *last = if *last == b'0' { b'1' } else { b'0' };
        let bad_hash = String::from_utf8(bad_hash).unwrap();
        assert!(!verify_token(&raw, &bad_hash));

        // Flip the last character of the presented token.
        let mut bad_raw = raw.clone().into_bytes();
        let last = bad_raw.last_mut().unwrap();
        *last = if *last == b'0' { b'1' } else { b'0' };
        let bad_raw = String::from_utf8(bad_raw).unwrap();
        assert!(!verify_token(&bad_raw, &hash));
    }
}
