//! Registry bearer tokens: the short-lived credential an OCI client buys at
//! `/v2/token` and spends on every pull.
//!
//! A port because the key is the interesting part. Today's is random per
//! process — a restart invalidates every outstanding token, which two
//! instances behind one address cannot afford — and a persisted-key
//! implementation has somewhere to land. It also lets the auth tests sign
//! with a key they chose.

use serde::{Deserialize, Serialize};

/// What a registry token carries: the user it was issued to (none for an
/// anonymous token) and its expiry. The scope is recorded for logging only;
/// permissions are checked against the database on every request. A token
/// carries its user's full rights on every route under the auth layer, even
/// when it was bought with an API token: `ApiToken.permissions_json` is not
/// enforced anywhere yet, and whoever enforces it must carry that scope here.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Claims {
    pub sub: Option<String>,
    pub exp: i64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub scope: Vec<String>,
    /// Issued to a static config token: the middleware resolves it to the
    /// same synthetic admin instead of looking `sub` up in the database.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub static_token: bool,
    /// The API token it was bought with: revoking that token revokes this one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_token_id: Option<String>,
}

pub trait RegistryTokenSigner: Send + Sync {
    fn sign(&self, claims: &Claims) -> String;
    /// The claims of a token whose signature holds and which has not expired.
    fn verify(&self, token: &str) -> Option<Claims>;
}
