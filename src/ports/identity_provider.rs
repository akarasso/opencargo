//! The outbound port to an identity provider, like `UpstreamStrategy` and
//! unnumbered (A1 C1): start an attempt, finish it into a verified identity,
//! probe reachability, end a session. The protocol lives in its adapter.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::domain::identity::{ExternalIdentity, ProviderProfile};

/// What one login attempt must remember until its callback: sealed into the
/// attempt's cookie by the caller, never stored server-side.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Attempt {
    pub provider: String,
    pub state: String,
    pub nonce: String,
    pub verifier: String,
}

/// The query of the provider's redirect back to us.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct Callback {
    pub code: Option<String>,
    pub state: Option<String>,
    pub iss: Option<String>,
    pub error: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum IdpFailure {
    /// The provider could not be reached or answered 5xx.
    Unavailable(String),
    /// The provider answered, and what it answered does not verify.
    Rejected(String),
    /// The provider refused the exchange (4xx, `error=` on the callback).
    IdpError(String),
}

impl std::fmt::Display for IdpFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unavailable(m) => write!(f, "identity provider unavailable: {m}"),
            Self::Rejected(m) => write!(f, "identity provider answer rejected: {m}"),
            Self::IdpError(m) => write!(f, "identity provider error: {m}"),
        }
    }
}

#[async_trait]
pub trait IdentityProvider: Send + Sync {
    fn profile(&self) -> &ProviderProfile;

    /// The URL to send the browser to, and the attempt to remember.
    async fn start(&self, redirect_uri: &str) -> Result<(String, Attempt), IdpFailure>;

    /// The callback's code exchanged and its ID Token verified against the
    /// attempt. The caller has already matched `state` to the attempt.
    async fn finish(
        &self,
        attempt: &Attempt,
        callback: &Callback,
        redirect_uri: &str,
    ) -> Result<ExternalIdentity, IdpFailure>;

    /// The server's own reachability check (discovery and keys); the only
    /// input of the reauthentication clock's suspension.
    async fn probe(&self) -> bool;

    /// Where to send the browser to end the provider's session, if it says.
    async fn end_session(&self, post_logout_redirect: &str) -> Option<String>;
}
