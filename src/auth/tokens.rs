use rand::Rng;
use sha2::{Digest, Sha256};

/// What a scoped credential is hashed with. A binary that predates scopes
/// hashes the raw value alone, so it cannot verify one of these: rolling the
/// image back refuses every scoped token instead of promoting it to a
/// credential that carries every right of its bearer.
const SCOPED_TAG: &str = "scoped:";

/// The form a scoped credential takes, derived from the configured prefix so
/// one setting still owns both: `trg_` issues `trgs_`.
pub fn scoped_prefix(prefix: &str) -> String {
    format!("{}s_", prefix.strip_suffix('_').unwrap_or(prefix))
}

/// Whether a presented value carries the scoped form.
pub fn is_scoped_form(raw: &str, prefix: &str) -> bool {
    raw.starts_with(&scoped_prefix(prefix))
}

/// Generate a new API token under the configured prefix, in the plain form or
/// the scoped one.
///
/// Returns `(raw_token, token_hash)`.
/// Token format: `{prefix}{random_32_hex}` (e.g., `trg_a1b2c3d4...`).
pub fn generate_token(prefix: &str, scoped: bool) -> (String, String) {
    let mut rng = rand::thread_rng();
    let random_bytes: [u8; 16] = rng.gen();
    let hex_part: String = random_bytes.iter().map(|b| format!("{b:02x}")).collect();
    let form = if scoped {
        scoped_prefix(prefix)
    } else {
        prefix.to_string()
    };
    let raw_token = format!("{form}{hex_part}");
    let hash = hash_credential(&raw_token, prefix);
    (raw_token, hash)
}

/// The hash stored for a credential, which its form decides.
pub fn hash_credential(token: &str, prefix: &str) -> String {
    if is_scoped_form(token, prefix) {
        hash_token(&format!("{SCOPED_TAG}{token}"))
    } else {
        hash_token(token)
    }
}

/// Compute a SHA-256 hex hash of the given token.
pub fn hash_token(token: &str) -> String {
    let digest = Sha256::digest(token.as_bytes());
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

/// Verify a presented credential of either form against its stored hash.
pub fn verify_credential(token: &str, hash: &str, prefix: &str) -> bool {
    constant_time(&hash_credential(token, prefix), hash)
}

/// Verify that a raw token matches a stored hash using constant-time comparison.
pub fn verify_token(token: &str, hash: &str) -> bool {
    constant_time(&hash_token(token), hash)
}

fn constant_time(computed: &str, hash: &str) -> bool {
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
        let (raw, hash) = generate_token("trg_", false);
        assert!(raw.starts_with("trg_"));
        assert_eq!(raw.len(), "trg_".len() + 32, "16 random bytes → 32 hex chars");
        assert!(raw["trg_".len()..].bytes().all(|b| b.is_ascii_hexdigit()));
        assert_eq!(hash.len(), 64, "SHA-256 → 64 hex chars");
        assert!(hash.bytes().all(|b| b.is_ascii_hexdigit()));
        assert_eq!(hash, hash_token(&raw), "returned hash is the hash of the raw token");
        assert!(verify_token(&raw, &hash));
    }

    /// The rollback property: what a binary without scopes computes for a
    /// scoped credential never matches what was stored for it, so the image
    /// going back refuses the token instead of honouring it in full.
    #[test]
    fn a_scoped_credential_carries_a_form_and_a_hash_an_older_binary_refuses() {
        let (raw, hash) = generate_token("trg_", true);
        assert!(raw.starts_with("trgs_"));
        assert!(!raw.starts_with("trg_"), "not the form an older binary looks for");
        assert!(verify_credential(&raw, &hash, "trg_"));
        assert!(!verify_token(&raw, &hash), "the untagged hash is not this one");

        let (plain, plain_hash) = generate_token("trg_", false);
        assert!(verify_credential(&plain, &plain_hash, "trg_"));
        assert!(verify_token(&plain, &plain_hash), "the plain form is unchanged");
    }

    #[test]
    fn two_generated_tokens_differ() {
        let (raw1, hash1) = generate_token("trg_", false);
        let (raw2, hash2) = generate_token("trg_", false);
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
        let (_raw, hash) = generate_token("trg_", false);
        let (other_raw, _) = generate_token("trg_", false);
        assert!(!verify_token(&other_raw, &hash));
        assert!(!verify_token("trg_deadbeef", &hash));
    }

    #[test]
    fn length_mismatch_is_rejected_without_panic() {
        let (raw, hash) = generate_token("trg_", false);
        assert!(!verify_token(&raw, ""));
        assert!(!verify_token(&raw, "abc"));
        assert!(!verify_token(&raw, &hash[..63]));
        assert!(!verify_token(&raw, &format!("{hash}00")));
        // Empty token still hashes to 64 chars — same length, wrong value.
        assert!(!verify_token("", &hash));
    }

    #[test]
    fn single_character_flip_is_rejected() {
        let (raw, hash) = generate_token("trg_", false);

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
