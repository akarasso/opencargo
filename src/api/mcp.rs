//! MCP governance administration.

use axum::{
    extract::{Path, State},
    Json,
};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::api::{require_admin, require_auth};
use crate::app::mcp::supervisor::run_locked;
use crate::app::mcp::sync::SyncError;
use crate::auth::middleware::AuthUser;
use crate::domain::{Format, RepoKind, Repository};
use crate::error::{AppError, AppResult};
use crate::server::AppState;

fn admin(request: &axum::http::Request<axum::body::Body>) -> AppResult<AuthUser> {
    let caller = require_auth(request)?;
    require_admin(&caller)?;
    Ok(caller)
}

async fn mcp_repo(state: &AppState, name: &str) -> AppResult<Repository> {
    let repo = crate::registry::load_repo(state.repos.as_ref(), name).await?;
    crate::registry::ensure_format(&repo, Format::Mcp)?;
    Ok(repo)
}

async fn body<T: for<'de> Deserialize<'de> + Default>(request: axum::http::Request<axum::body::Body>) -> AppResult<T> {
    let bytes = axum::body::to_bytes(request.into_body(), 1 << 20)
        .await
        .map_err(|e| AppError::BadRequest(format!("failed to read body: {e}")))?;
    if bytes.is_empty() {
        return Ok(T::default());
    }
    Ok(serde_json::from_slice(&bytes)?)
}

#[derive(Deserialize, Default)]
pub struct ProbeRequest {
    pub name: Option<String>,
    pub version: Option<String>,
}

/// POST /api/v1/mcp/{repo}/probe -- probe the latest versions' remotes now,
/// or one version's; whether or not the repository probes on a schedule.
pub async fn probe(
    State(state): State<AppState>,
    Path(name): Path<String>,
    request: axum::http::Request<axum::body::Body>,
) -> AppResult<Json<Value>> {
    admin(&request)?;
    let repo = mcp_repo(&state, &name).await?;
    let request: ProbeRequest = body(request).await?;
    let only = match (&request.name, &request.version) {
        (Some(server), version) => Some(
            state
                .mcp
                .version(repo.id, repo.id, server, version.as_deref())
                .await?
                .ok_or_else(|| AppError::NotFound(format!("server not found: {server}")))?
                .version_id,
        ),
        _ => None,
    };
    let cfg = state.mcp_settings.get(&name).cloned().unwrap_or_default();
    let settings = crate::server::probe_settings_of(&cfg).map_err(|e| AppError::Internal(e.to_string()))?;
    let report = state.probe_mirror().run(repo.id, &settings, true, only).await?;
    Ok(Json(json!({"repository": name, "report": report})))
}

#[derive(Deserialize, Default)]
pub struct SyncRequest {
    #[serde(default)]
    pub full: bool,
}

/// POST /api/v1/mcp/{repo}/sync -- one run now, under the same lock as the
/// scheduled one; answers the run's report.
pub async fn sync(
    State(state): State<AppState>,
    Path(name): Path<String>,
    request: axum::http::Request<axum::body::Body>,
) -> AppResult<Json<Value>> {
    admin(&request)?;
    let repo = mcp_repo(&state, &name).await?;
    if repo.kind()? != RepoKind::Proxy {
        return Err(AppError::NotFound(format!("{name} is not a mirror")));
    }
    let upstream = repo
        .upstream_url
        .clone()
        .ok_or_else(|| AppError::BadRequest(format!("{name} has no upstream")))?;
    let request: SyncRequest = body(request).await?;
    let report = run_locked(&state.mcp_sync, &state.sync_mirror(), repo.id, &upstream, request.full)
        .await
        .map_err(|e| match e {
            SyncError::Store(e) => AppError::from(e),
            other => AppError::BadGateway(other.to_string()),
        })?;
    Ok(Json(json!({"repository": name, "report": report})))
}
