use axum::{
    extract::{Query, State},
    http::{header, HeaderMap, HeaderValue, Method, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use chrono::SecondsFormat;
use hmac::{Hmac, Mac};
use rand::Rng;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::Sha256;

use crate::auth::middleware::{
    authenticate_basic, authenticate_bearer, basic_credentials, AuthFailure, AuthState, AuthUser,
};
use crate::error::AppError;
use crate::server::AppState;

/// Every registry token starts with this, so the middleware can tell one
/// from a static or API token without a database lookup.
pub const PREFIX: &str = "ocr_";
pub const SERVICE: &str = "opencargo";
const TTL_SECS: i64 = 3600;
const MAX_SCOPES: usize = 16;
const MAX_SCOPE_LEN: usize = 256;

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

/// Signs and verifies registry tokens with a key that lives for one process:
/// a restart invalidates every outstanding token, which is fine for a
/// one-hour credential the client re-requests on any 401.
#[derive(Clone)]
pub struct TokenSigner {
    key: [u8; 32],
}

impl TokenSigner {
    pub fn random() -> Self {
        let mut key = [0u8; 32];
        rand::thread_rng().fill(&mut key);
        Self { key }
    }

    /// `ocr_` + base64url(claims JSON) + `.` + base64url(HMAC-SHA256).
    pub fn sign(&self, claims: &Claims) -> String {
        let payload = URL_SAFE_NO_PAD.encode(serde_json::to_vec(claims).unwrap_or_default());
        let mac = self.mac().chain_update(&payload).finalize().into_bytes();
        format!("{PREFIX}{payload}.{}", URL_SAFE_NO_PAD.encode(mac))
    }

    /// The claims of a token whose signature holds and which has not expired.
    pub fn verify(&self, token: &str) -> Option<Claims> {
        let (payload, tag) = token.strip_prefix(PREFIX)?.split_once('.')?;
        let tag = URL_SAFE_NO_PAD.decode(tag).ok()?;
        self.mac().chain_update(payload).verify_slice(&tag).ok()?;
        let claims: Claims = serde_json::from_slice(&URL_SAFE_NO_PAD.decode(payload).ok()?).ok()?;
        (claims.exp > chrono::Utc::now().timestamp()).then_some(claims)
    }

    fn mac(&self) -> Hmac<Sha256> {
        Hmac::<Sha256>::new_from_slice(&self.key).expect("HMAC accepts any key length")
    }
}

/// `GET /v2/token?service=opencargo&scope=repository:{repo}/{name}:pull`.
/// Basic credentials or an API token identify the caller; without any,
/// an anonymous token is issued when `anonymous_read` allows it.
pub async fn issue_token(
    State(state): State<AppState>,
    Query(params): Query<Vec<(String, String)>>,
    headers: HeaderMap,
) -> Result<Response, Response> {
    let user = caller(&state.auth, &headers).await?;
    if user.is_none() && !state.auth.anonymous_read {
        return Err(unauthorized("authentication required"));
    }
    let api_token_id = match user.as_ref().and(bearer_value(&headers)) {
        Some(raw) if !raw.starts_with("ocr_") => {
            crate::auth::middleware::live_api_token(&state.auth, raw)
                .await
                .map_err(|e| {
                    tracing::warn!(error = %e, "store error during token endpoint authentication");
                    AppError::ServiceUnavailable("authentication temporarily unavailable, try again".to_string()).into_response()
                })?
                .map(|t| t.id)
        }
        _ => None,
    };
    let issued_at = chrono::Utc::now();
    let claims = Claims {
        sub: user.as_ref().map(|u| u.username.clone()),
        exp: issued_at.timestamp() + TTL_SECS,
        scope: params
            .into_iter()
            .filter(|(key, value)| key == "scope" && value.len() <= MAX_SCOPE_LEN)
            .map(|(_, value)| value)
            .take(MAX_SCOPES)
            .collect(),
        static_token: user.as_ref().is_some_and(|u| u.user_id.is_none()),
        api_token_id,
    };
    tracing::info!(
        subject = claims.sub.as_deref().unwrap_or("anonymous"),
        scope = ?claims.scope,
        "registry token issued"
    );
    let token = state.auth.registry_tokens.sign(&claims);
    Ok(Json(json!({
        "token": token,
        "access_token": token,
        "expires_in": TTL_SECS,
        "issued_at": issued_at.to_rfc3339_opts(SecondsFormat::Secs, true),
    }))
    .into_response())
}

fn bearer_value(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
}

/// The user behind the `Authorization` header, `None` when there is none;
/// credentials that are present but wrong are an error, never anonymous.
async fn caller(auth: &AuthState, headers: &HeaderMap) -> Result<Option<AuthUser>, Response> {
    let Some(value) = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
    else {
        return Ok(None);
    };
    let outcome = if let Some((username, password)) = basic_credentials(value) {
        authenticate_basic(auth, &username, &password).await
    } else if let Some(token) = value.strip_prefix("Bearer ") {
        authenticate_bearer(auth, token).await.map_err(|e| {
            tracing::warn!(error = %e, "database error during token endpoint authentication");
            AuthFailure::Unavailable
        })
    } else {
        Ok(None)
    };
    match outcome {
        Ok(Some(user)) => Ok(Some(user)),
        Ok(None) => Err(unauthorized("invalid credentials")),
        Err(AuthFailure::Throttled) => Err(AppError::TooManyRequests(
            "too many authentication attempts, try again later".to_string(),
        )
        .into_response()),
        Err(AuthFailure::Unavailable) => Err(AppError::ServiceUnavailable(
            "authentication temporarily unavailable, try again".to_string(),
        )
        .into_response()),
    }
}

/// A 401 in the distribution error shape, which Docker surfaces to the user;
/// no challenge, since the client is already at the realm.
fn unauthorized(message: &str) -> Response {
    (
        StatusCode::UNAUTHORIZED,
        Json(json!({"errors": [{"code": "UNAUTHORIZED", "message": message}]})),
    )
        .into_response()
}

/// The `WWW-Authenticate` value for a 401 on `path`: the token realm, plus
/// the image's scope when the path names one.
pub fn challenge(base_url: &str, method: &Method, path: &str) -> Option<HeaderValue> {
    let mut value = format!(
        "Bearer realm=\"{}/v2/token\",service=\"{SERVICE}\"",
        base_url.trim_end_matches('/')
    );
    if let Some(image) = image_in_path(path) {
        let action = if matches!(*method, Method::GET | Method::HEAD) {
            "pull"
        } else {
            "push"
        };
        value.push_str(&format!(",scope=\"repository:{image}:{action}\""));
    }
    HeaderValue::from_str(&value).ok()
}

/// `{repo}/{name}` when `path` is an image route; the name arrives folded into
/// one percent-encoded segment by the pre-route rewrite.
fn image_in_path(path: &str) -> Option<String> {
    let segs: Vec<&str> = path.strip_prefix("/v2/")?.split('/').collect();
    let (_, end) = super::routing::split_v2_path(&segs)?;
    Some(
        segs[..end]
            .join("/")
            .replace("%2F", "/")
            .replace("%2f", "/"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn claims(exp: i64) -> Claims {
        Claims {
            sub: Some("dev".to_string()),
            exp,
            scope: vec!["repository:r/app:pull".to_string()],
            static_token: false,
            api_token_id: None,
        }
    }

    #[test]
    fn signed_token_round_trips() {
        let signer = TokenSigner::random();
        let token = signer.sign(&claims(i64::MAX));
        assert!(token.starts_with(PREFIX));
        let verified = signer.verify(&token).expect("valid token");
        assert_eq!(verified.sub.as_deref(), Some("dev"));
        assert_eq!(verified.scope, ["repository:r/app:pull"]);
        let anonymous = signer.sign(&Claims {
            sub: None,
            exp: i64::MAX,
            scope: Vec::new(),
            static_token: false,
            api_token_id: None,
        });
        let verified = signer.verify(&anonymous).expect("valid");
        assert!(verified.sub.is_none() && !verified.static_token);
        let admin = signer.sign(&Claims {
            static_token: true,
            ..claims(i64::MAX)
        });
        assert!(signer.verify(&admin).expect("valid").static_token);
    }

    #[test]
    fn expired_tampered_or_foreign_tokens_are_refused() {
        let signer = TokenSigner::random();
        let expired = signer.sign(&claims(chrono::Utc::now().timestamp() - 1));
        assert!(signer.verify(&expired).is_none(), "expired");
        let token = signer.sign(&claims(i64::MAX));
        assert!(TokenSigner::random().verify(&token).is_none(), "other key");
        let (payload, tag) = token.split_once('.').unwrap();
        let forged = format!("{}.{tag}", &payload[..payload.len() - 1]);
        assert!(signer.verify(&forged).is_none(), "payload edited");
        let mut flipped = token.clone().into_bytes();
        let last = flipped.last_mut().unwrap();
        *last = if *last == b'A' { b'B' } else { b'A' };
        assert!(
            signer
                .verify(&String::from_utf8(flipped).unwrap())
                .is_none(),
            "mac edited"
        );
        for junk in ["ocr_", "ocr_abc", "ocr_abc.def", "trg_abc.def", ""] {
            assert!(signer.verify(junk).is_none(), "{junk:?}");
        }
    }

    #[test]
    fn challenge_names_the_realm_and_the_image() {
        let realm = |method: Method, path: &str| {
            challenge("http://r.example/", &method, path)
                .unwrap()
                .to_str()
                .unwrap()
                .to_string()
        };
        let base = "Bearer realm=\"http://r.example/v2/token\",service=\"opencargo\"";
        assert_eq!(realm(Method::GET, "/v2/"), base);
        assert_eq!(realm(Method::GET, "/v2/token"), base);
        assert_eq!(
            realm(Method::GET, "/v2/r/team%2Fapp/manifests/latest"),
            format!("{base},scope=\"repository:r/team/app:pull\"")
        );
        assert_eq!(
            realm(Method::HEAD, "/v2/r/app/blobs/sha256:abc"),
            format!("{base},scope=\"repository:r/app:pull\"")
        );
        assert_eq!(
            realm(Method::POST, "/v2/r/app/blobs/uploads/"),
            format!("{base},scope=\"repository:r/app:push\"")
        );
        assert_eq!(realm(Method::GET, "/v2/r/manifests/latest"), base);
    }
}
