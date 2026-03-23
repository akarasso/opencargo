use axum::{
    extract::State,
    http::{Request, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
    Json,
};
use base64::Engine;
use serde_json::json;
use sqlx::SqlitePool;
use std::sync::Arc;

use super::tokens;

// ---------------------------------------------------------------------------
// Auth state (passed via axum's State extractor)
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct AuthState {
    pub static_tokens: Vec<String>,
    pub anonymous_read: bool,
    pub db: SqlitePool,
}

// ---------------------------------------------------------------------------
// Authenticated user, inserted into request extensions on success
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct AuthUser {
    pub token: String,
    pub user_id: Option<i64>,
    pub username: String,
    pub role: String,
}

// ---------------------------------------------------------------------------
// Middleware
// ---------------------------------------------------------------------------

/// Axum middleware for Bearer token authentication.
///
/// Supports both static tokens from config (backwards compat) and
/// DB-backed API tokens with user lookup.
pub async fn auth_middleware(
    State(state): State<Arc<AuthState>>,
    request: Request<axum::body::Body>,
    next: Next,
) -> Response {
    let is_read = request.method() == axum::http::Method::GET
        || request.method() == axum::http::Method::HEAD;

    // Detect if this is an OCI request (Docker client) for proper Www-Authenticate headers
    let is_oci = request.uri().path().starts_with("/v2");

    // Try to extract the Bearer token from the Authorization header.
    let bearer_token = request
        .headers()
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .map(|t| t.to_string());

    // Try Basic Auth if no Bearer token
    let basic_auth = if bearer_token.is_none() {
        request
            .headers()
            .get(axum::http::header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Basic "))
            .and_then(|b64| {
                base64::engine::general_purpose::STANDARD.decode(b64).ok()
            })
            .and_then(|bytes| String::from_utf8(bytes).ok())
            .and_then(|decoded| {
                let parts: Vec<&str> = decoded.splitn(2, ':').collect();
                if parts.len() == 2 {
                    Some((parts[0].to_string(), parts[1].to_string()))
                } else {
                    None
                }
            })
    } else {
        None
    };

    // Handle Basic Auth
    if let Some((username, password)) = basic_auth {
        if let Ok(Some(user)) = crate::db::get_user_by_username(&state.db, &username).await {
            let password_ok = super::users::verify_password(&password, &user.password_hash)
                .unwrap_or(false);
            if password_ok {
                let mut request = request;
                request.extensions_mut().insert(AuthUser {
                    token: String::new(),
                    user_id: Some(user.id),
                    username: user.username,
                    role: user.role,
                });
                return next.run(request).await;
            }
        }
        // Basic Auth provided but invalid
        return unauthorized_response(is_oci);
    }

    match bearer_token {
        Some(t) => {
            // 1. Check static tokens first (backwards compatibility)
            // Use constant-time comparison to prevent timing attacks
            let is_static = state.static_tokens.iter().any(|st| {
                st.len() == t.len()
                    && st
                        .bytes()
                        .zip(t.bytes())
                        .fold(0u8, |acc, (a, b)| acc | (a ^ b))
                        == 0
            });
            if is_static {
                let mut request = request;
                request.extensions_mut().insert(AuthUser {
                    token: t,
                    user_id: None,
                    username: "static-token".to_string(),
                    role: "admin".to_string(),
                });
                return next.run(request).await;
            }

            // 2. Try DB token lookup: extract the prefix (first 8 chars of the token)
            //    Token format: {prefix_str}{32_hex} — prefix is everything up to and
            //    including the first underscore + the first 4 hex chars.
            //    Actually, the prefix stored is the first 8 chars of the raw token.
            if let Some(auth_user) = try_db_token_auth(&state.db, &t).await {
                let mut request = request;
                request.extensions_mut().insert(auth_user);
                return next.run(request).await;
            }

            // Token not valid
            if state.anonymous_read && is_read {
                next.run(request).await
            } else {
                unauthorized_response(is_oci)
            }
        }
        None => {
            // No token. Allow anonymous GET if configured.
            if state.anonymous_read && is_read {
                next.run(request).await
            } else {
                unauthorized_response(is_oci)
            }
        }
    }
}

/// Build an unauthorized response. For OCI/Docker requests, include the
/// `Www-Authenticate` header so Docker knows to send credentials.
fn unauthorized_response(is_oci: bool) -> Response {
    if is_oci {
        (
            StatusCode::UNAUTHORIZED,
            [(axum::http::header::WWW_AUTHENTICATE, "Basic realm=\"opencargo\"")],
            Json(json!({
                "errors": [{"code": "UNAUTHORIZED", "message": "authentication required"}]
            })),
        )
            .into_response()
    } else {
        (
            StatusCode::UNAUTHORIZED,
            Json(json!({
                "error": "Invalid or missing authentication token"
            })),
        )
            .into_response()
    }
}

/// Attempt to authenticate via a DB API token.
///
/// Looks up the token by its prefix, verifies the hash, checks expiration,
/// loads the user, and updates `last_used_at`.
async fn try_db_token_auth(db: &SqlitePool, raw_token: &str) -> Option<AuthUser> {
    // The prefix stored in DB is the first 16 characters of the raw token.
    if raw_token.len() < 16 {
        return None;
    }
    let prefix = &raw_token[..16];

    let db_token = crate::db::get_token_by_prefix(db, prefix).await.ok()??;

    // Verify the token hash
    if !tokens::verify_token(raw_token, &db_token.token_hash) {
        return None;
    }

    // Check expiration
    if let Some(ref expires_at) = db_token.expires_at {
        if let Ok(exp) = chrono::NaiveDateTime::parse_from_str(expires_at, "%Y-%m-%d %H:%M:%S") {
            if exp < chrono::Utc::now().naive_utc() {
                return None;
            }
        }
    }

    // Load the user
    let user = sqlx::query_as::<_, crate::db::User>("SELECT * FROM users WHERE id = ?1")
        .bind(db_token.user_id)
        .fetch_optional(db)
        .await
        .ok()??;

    // Update last_used_at (fire-and-forget)
    let _ = crate::db::update_token_last_used(db, &db_token.id).await;

    Some(AuthUser {
        token: raw_token.to_string(),
        user_id: Some(user.id),
        username: user.username,
        role: user.role,
    })
}
