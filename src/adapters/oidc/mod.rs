//! OpenID Connect behind `IdentityProvider`: authorization code with PKCE
//! S256 and a nonce, `response_mode=query`, asymmetric signatures only.
//! Discovery and keys are fetched lazily, one flight at a time, with a
//! negative cache, so a provider that is down never slows the registry.

pub mod profiles;

use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use base64::Engine;
use jsonwebtoken::jwk::JwkSet;
use jsonwebtoken::{Algorithm, DecodingKey, Validation};
use rand::RngCore;
use serde::Deserialize;
use serde_json::{Map, Value};
use sha2::Digest;
use tokio::sync::Mutex;

use crate::domain::identity::{ExternalIdentity, ProviderProfile};
use crate::ports::identity_provider::{Attempt, Callback, IdentityProvider, IdpFailure};
use profiles::{Issuers, Kind};

const DISCOVERY_TTL: Duration = Duration::from_secs(3600);
const NEGATIVE_TTL: Duration = Duration::from_secs(30);
const LEEWAY_SECS: u64 = 60;

const ASYMMETRIC: &[Algorithm] = &[
    Algorithm::RS256,
    Algorithm::RS384,
    Algorithm::RS512,
    Algorithm::PS256,
    Algorithm::PS384,
    Algorithm::PS512,
    Algorithm::ES256,
    Algorithm::ES384,
    Algorithm::EdDSA,
];

pub struct OidcSettings {
    pub name: String,
    pub kind: Kind,
    /// Google's constant, an Entra template with `{tid}`, or the declared URL.
    pub issuer: String,
    pub client_id: String,
    pub client_secret: String,
    pub extra_scopes: Vec<String>,
    pub timeout: Duration,
    /// The shortest interval between two key-set fetches forced by an
    /// unknown `kid`.
    pub jwks_refresh_floor: Duration,
}

#[derive(Clone, Debug, Deserialize)]
struct Discovery {
    issuer: String,
    authorization_endpoint: String,
    token_endpoint: String,
    jwks_uri: String,
    end_session_endpoint: Option<String>,
    #[serde(default)]
    authorization_response_iss_parameter_supported: bool,
    token_endpoint_auth_methods_supported: Option<Vec<String>>,
}

struct Cached<T> {
    value: Option<(Arc<T>, Instant)>,
    failed_at: Option<Instant>,
}

impl<T> Default for Cached<T> {
    fn default() -> Self {
        Self {
            value: None,
            failed_at: None,
        }
    }
}

pub struct OidcProvider {
    settings: OidcSettings,
    issuers: Issuers,
    profile: ProviderProfile,
    http: reqwest::Client,
    discovery: Mutex<Cached<Discovery>>,
    jwks: Mutex<Cached<JwkSet>>,
}

fn random_token() -> String {
    let mut bytes = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut bytes);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

fn challenge(verifier: &str) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(sha2::Sha256::digest(verifier.as_bytes()))
}

fn transport(err: reqwest::Error) -> IdpFailure {
    IdpFailure::Unavailable(err.without_url().to_string())
}

impl OidcProvider {
    /// `profile` carries the operator's rules; its authority is recomputed
    /// here from the provider type.
    pub fn new(settings: OidcSettings, mut profile: ProviderProfile) -> anyhow::Result<Self> {
        let issuers = profiles::issuers(&settings.kind, &settings.issuer);
        profile.authority =
            crate::domain::identity::Authority::new(&settings.name, &issuers.authority_issuer);
        profile.open = settings.kind.open(&settings.issuer);
        profile.tenant_pinned = settings.kind.tenant_pinned();
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
        let http = reqwest::Client::builder()
            .connect_timeout(settings.timeout)
            .timeout(settings.timeout)
            .redirect(reqwest::redirect::Policy::none())
            .build()?;
        Ok(Self {
            settings,
            issuers,
            profile,
            http,
            discovery: Mutex::new(Cached::default()),
            jwks: Mutex::new(Cached::default()),
        })
    }

    async fn get_json<T: serde::de::DeserializeOwned>(&self, url: &str) -> Result<T, IdpFailure> {
        let resp = self.http.get(url).send().await.map_err(transport)?;
        if !resp.status().is_success() {
            return Err(IdpFailure::Unavailable(format!("{url}: {}", resp.status())));
        }
        resp.json().await.map_err(transport)
    }

    async fn fetch_discovery(&self) -> Result<Discovery, IdpFailure> {
        let url = format!(
            "{}/.well-known/openid-configuration",
            self.issuers.discovery_base.trim_end_matches('/')
        );
        let mut d: Discovery = self.get_json(&url).await?;
        if d.issuer.trim_end_matches('/') != self.issuers.discovery_issuer.trim_end_matches('/') {
            return Err(IdpFailure::Rejected(format!(
                "discovery announces issuer {}, expected {}",
                d.issuer, self.issuers.discovery_issuer
            )));
        }
        d.issuer = self.issuers.discovery_issuer.clone();
        Ok(d)
    }

    /// One flight at a time: callers queue on the lock and find the fresh
    /// value; a recent failure answers at once without calling out.
    async fn discovery(&self, fresh: bool) -> Result<Arc<Discovery>, IdpFailure> {
        let mut cache = self.discovery.lock().await;
        if !fresh {
            if let Some((d, at)) = &cache.value {
                if at.elapsed() < DISCOVERY_TTL {
                    return Ok(d.clone());
                }
            }
            if cache
                .failed_at
                .is_some_and(|at| at.elapsed() < NEGATIVE_TTL)
            {
                return Err(IdpFailure::Unavailable("discovery failed recently".into()));
            }
        }
        match self.fetch_discovery().await {
            Ok(d) => {
                let d = Arc::new(d);
                cache.value = Some((d.clone(), Instant::now()));
                cache.failed_at = None;
                Ok(d)
            }
            Err(e) => {
                cache.failed_at = Some(Instant::now());
                Err(e)
            }
        }
    }

    /// The key set, refetched when stale, when forced by the probe, or when
    /// a token names a key it does not hold (rotation), at most once per
    /// floor interval.
    async fn keys(&self, want_kid: Option<&str>, fresh: bool) -> Result<Arc<JwkSet>, IdpFailure> {
        let discovery = self.discovery(fresh).await?;
        let mut cache = self.jwks.lock().await;
        if !fresh {
            if let Some((set, at)) = &cache.value {
                let has = want_kid.is_none_or(|kid| set.find(kid).is_some());
                if (has && at.elapsed() < DISCOVERY_TTL)
                    || at.elapsed() < self.settings.jwks_refresh_floor
                {
                    return Ok(set.clone());
                }
            }
            if cache
                .failed_at
                .is_some_and(|at| at.elapsed() < NEGATIVE_TTL)
            {
                return Err(IdpFailure::Unavailable("key set failed recently".into()));
            }
        }
        match self.get_json::<JwkSet>(&discovery.jwks_uri).await {
            Ok(set) => {
                let set = Arc::new(set);
                cache.value = Some((set.clone(), Instant::now()));
                cache.failed_at = None;
                Ok(set)
            }
            Err(e) => {
                cache.failed_at = Some(Instant::now());
                Err(e)
            }
        }
    }

    async fn exchange(
        &self,
        discovery: &Discovery,
        code: &str,
        verifier: &str,
        redirect_uri: &str,
    ) -> Result<String, IdpFailure> {
        let mut form = vec![
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", redirect_uri),
            ("code_verifier", verifier),
            ("client_id", self.settings.client_id.as_str()),
        ];
        let basic = discovery
            .token_endpoint_auth_methods_supported
            .as_ref()
            .is_some_and(|m| {
                m.iter().any(|x| x == "client_secret_basic")
                    && !m.iter().any(|x| x == "client_secret_post")
            });
        let mut request = self.http.post(&discovery.token_endpoint);
        if basic {
            request =
                request.basic_auth(&self.settings.client_id, Some(&self.settings.client_secret));
        } else {
            form.push(("client_secret", self.settings.client_secret.as_str()));
        }
        let resp = request.form(&form).send().await.map_err(transport)?;
        let status = resp.status();
        if status.is_server_error() {
            return Err(IdpFailure::Unavailable(format!("token endpoint: {status}")));
        }
        if !status.is_success() {
            return Err(IdpFailure::IdpError(format!("token endpoint: {status}")));
        }
        #[derive(Deserialize)]
        struct TokenResponse {
            id_token: Option<String>,
        }
        let body: TokenResponse = resp.json().await.map_err(transport)?;
        body.id_token
            .ok_or_else(|| IdpFailure::Rejected("no id_token in the token response".into()))
    }

    async fn verify(&self, id_token: &str, nonce: &str) -> Result<Map<String, Value>, IdpFailure> {
        let reject = |m: String| IdpFailure::Rejected(m);
        let header = jsonwebtoken::decode_header(id_token).map_err(|e| reject(e.to_string()))?;
        if !ASYMMETRIC.contains(&header.alg) {
            return Err(reject(format!("algorithm {:?} refused", header.alg)));
        }
        let keys = self.keys(header.kid.as_deref(), false).await?;
        let jwk = match header.kid.as_deref() {
            Some(kid) => keys.find(kid),
            None if keys.keys.len() == 1 => keys.keys.first(),
            None => None,
        }
        .ok_or_else(|| reject("no key for the token's kid".into()))?;
        let key = DecodingKey::from_jwk(jwk).map_err(|e| reject(e.to_string()))?;
        let mut validation = Validation::new(header.alg);
        validation.leeway = LEEWAY_SECS;
        validation.set_audience(&[self.settings.client_id.as_str()]);
        validation.set_required_spec_claims(&["exp", "iss", "sub", "aud"]);
        let data = jsonwebtoken::decode::<Map<String, Value>>(id_token, &key, &validation)
            .map_err(|e| reject(e.to_string()))?;
        let claims = data.claims;
        if claims.get("nonce").and_then(Value::as_str) != Some(nonce) {
            return Err(reject("nonce mismatch".into()));
        }
        if let Some(Value::Array(aud)) = claims.get("aud") {
            if aud.len() > 1
                && claims.get("azp").and_then(Value::as_str)
                    != Some(self.settings.client_id.as_str())
            {
                return Err(reject("azp is not this client".into()));
            }
        }
        Ok(claims)
    }
}

#[async_trait]
impl IdentityProvider for OidcProvider {
    fn profile(&self) -> &ProviderProfile {
        &self.profile
    }

    async fn start(&self, redirect_uri: &str) -> Result<(String, Attempt), IdpFailure> {
        let discovery = self.discovery(false).await?;
        let attempt = Attempt {
            provider: self.settings.name.clone(),
            state: random_token(),
            nonce: random_token(),
            verifier: random_token(),
        };
        let mut scopes = vec!["openid".to_string(), "email".into(), "profile".into()];
        scopes.extend(self.settings.extra_scopes.iter().cloned());
        let mut url = url::Url::parse(&discovery.authorization_endpoint)
            .map_err(|e| IdpFailure::Rejected(e.to_string()))?;
        url.query_pairs_mut()
            .append_pair("response_type", "code")
            .append_pair("response_mode", "query")
            .append_pair("client_id", &self.settings.client_id)
            .append_pair("redirect_uri", redirect_uri)
            .append_pair("scope", &scopes.join(" "))
            .append_pair("state", &attempt.state)
            .append_pair("nonce", &attempt.nonce)
            .append_pair("code_challenge", &challenge(&attempt.verifier))
            .append_pair("code_challenge_method", "S256");
        Ok((url.into(), attempt))
    }

    async fn finish(
        &self,
        attempt: &Attempt,
        callback: &Callback,
        redirect_uri: &str,
    ) -> Result<ExternalIdentity, IdpFailure> {
        if let Some(error) = &callback.error {
            return Err(IdpFailure::IdpError(error.clone()));
        }
        let code = callback
            .code
            .as_deref()
            .ok_or_else(|| IdpFailure::Rejected("no code".into()))?;
        let discovery = self.discovery(false).await?;
        if discovery.authorization_response_iss_parameter_supported {
            let expected = match &self.settings.kind {
                Kind::Entra { .. } if !self.settings.kind.tenant_pinned() => None,
                _ => Some(discovery.issuer.trim_end_matches('/')),
            };
            let got = callback.iss.as_deref().map(|i| i.trim_end_matches('/'));
            let ok = match (got, expected) {
                (Some(got), Some(expected)) => got == expected,
                (Some(_), None) => true,
                (None, _) => false,
            };
            if !ok {
                return Err(IdpFailure::Rejected(
                    "iss parameter missing or foreign (RFC 9207)".into(),
                ));
            }
        }
        let id_token = self
            .exchange(&discovery, code, &attempt.verifier, redirect_uri)
            .await?;
        let claims = self.verify(&id_token, &attempt.nonce).await?;
        profiles::project(
            &self.settings.kind,
            &self.settings.name,
            &self.settings.issuer,
            &self.issuers,
            &claims,
        )
        .map_err(IdpFailure::Rejected)
    }

    async fn probe(&self) -> bool {
        self.keys(None, true).await.is_ok()
    }

    async fn end_session(&self, post_logout_redirect: &str) -> Option<String> {
        let discovery = self.discovery(false).await.ok()?;
        let mut url = url::Url::parse(discovery.end_session_endpoint.as_deref()?).ok()?;
        url.query_pairs_mut()
            .append_pair("client_id", &self.settings.client_id)
            .append_pair("post_logout_redirect_uri", post_logout_redirect);
        Some(url.into())
    }
}

#[cfg(test)]
#[path = "tests.rs"]
mod tests;
