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
