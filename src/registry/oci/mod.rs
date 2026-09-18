pub mod blobs;
pub mod leaves;
pub mod manifests;
pub mod paths;
pub mod refs;
pub mod routes;
pub mod routing;
pub mod tags;
pub mod token;
pub mod uploads;
pub mod upstream;

use std::collections::HashMap;

use axum::{
    http::{HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use serde_json::json;
use sha2::Digest;

use crate::auth::middleware::AuthUser;
use crate::error::{AppError, AppResult};
use crate::proxy::Payload;
use crate::server::AppState;

use upstream::DOCKER_CONTENT_DIGEST;

/// The most tags one listing carries, locally and when asked of an upstream.
pub const MAX_TAGS: usize = 10_000;

/// `sha256:{hex}` of `data`, the wire form of every OCI digest.
fn sha256_digest(data: &[u8]) -> String {
    format!("sha256:{:x}", sha2::Sha256::digest(data))
}

fn is_digest(reference: &str) -> bool {
    reference.starts_with("sha256:")
}

/// A digest lands in cache keys and upstream URLs, so it must be exactly
/// `sha256:` plus 64 lowercase hex digits before anything is built from it.
fn parse_digest(digest: &str) -> AppResult<String> {
    let hex = digest.strip_prefix("sha256:").unwrap_or_default();
    if hex.len() != 64 || !hex.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
        return Err(AppError::BadRequest(format!("invalid digest: '{digest}'")));
    }
    Ok(digest.to_string())
}

fn param<'a>(params: &'a HashMap<String, String>, key: &str) -> AppResult<&'a str> {
    params
        .get(key)
        .map(String::as_str)
        .ok_or_else(|| AppError::BadRequest(format!("missing {key}")))
}

/// The `{repo}/{name}` every `/v2` route addresses; `name` may be nested
/// (`team/app`) and is validated before any key, path or URL is built.
pub struct OciRef {
    pub repo: String,
    pub name: String,
}

impl OciRef {
    pub fn parse(params: &HashMap<String, String>) -> AppResult<Self> {
        let repo = param(params, "repo")?;
        let name = param(params, "name")?;
        crate::registry::rules::rules_of(crate::domain::Format::Oci)?.validate(name)?;
        Ok(Self {
            repo: repo.to_string(),
            name: name.to_string(),
        })
    }

    /// `{repo}/{name}`, the key manifest paths and tag listings carry.
    pub fn image_name(&self) -> String {
        format!("{}/{}", self.repo, self.name)
    }
}


/// Serve a resolved blob or manifest; `Docker-Content-Digest` is whatever the
/// leaf derived from the content, never the client's reference.
async fn respond(state: &AppState, payload: Payload) -> AppResult<Response> {
    let mut extra = Vec::new();
    if let Some(digest) = &payload.digest {
        let value = HeaderValue::from_str(digest)
            .map_err(|_| AppError::Internal(format!("invalid digest header: {digest}")))?;
        extra.push((DOCKER_CONTENT_DIGEST, value));
    }
    state.proxy.stream_response(&payload, extra).await
}

/// `GET /v2/`: 200 for any caller the middleware identified, including an
/// anonymous registry token; a request without credentials is 401 so classic
/// Docker clients learn the token realm from the challenge before pushing.
pub async fn api_version_check(
    auth: Option<axum::Extension<AuthUser>>,
    claims: Option<axum::Extension<token::Claims>>,
) -> impl IntoResponse {
    let (status, body) = if auth.is_some() || claims.is_some() {
        (StatusCode::OK, json!({}))
    } else {
        (
            StatusCode::UNAUTHORIZED,
            json!({"errors": [{"code": "UNAUTHORIZED", "message": "authentication required"}]}),
        )
    };
    (
        status,
        [("Docker-Distribution-Api-Version", "registry/2.0")],
        Json(body),
    )
}
