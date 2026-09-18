//! `PackagePublish/2.0.0`: push, delete (= unlist) and relist.
//!
//! The push body is multipart and its first file field is the `.nupkg`,
//! spooled in memory under a cap: the spool is private to the request and is
//! what `place_shared` replays from when a commit is superseded.

use axum::{
    extract::{Multipart, Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use bytes::{Bytes, BytesMut};
use serde_json::json;
use tracing::info;

use crate::app::nuget::{NugetDependency, NugetPush, PublishNugetPackage};
use crate::app::publish_tail::Published;
use crate::app::releases::Yank;
use crate::auth::middleware::AuthUser;
use crate::domain::{Format, Repository};
use crate::error::{AppError, AppResult};
use crate::server::AppState;

use super::model::HostedFacts;
use super::nuspec;
use super::read::{id_of, key_of};
use super::version::NuGetVersion;

pub const MAX_NUPKG_BYTES: usize = 250 * 1024 * 1024;

fn require_user(auth: Option<axum::Extension<AuthUser>>) -> AppResult<AuthUser> {
    auth.map(|e| e.0)
        .ok_or_else(|| AppError::Unauthorized("authentication required".to_string()))
}

async fn writable(state: &AppState, repo_name: &str, user: &AuthUser) -> AppResult<Repository> {
    let repo = crate::registry::load_repo(state.repos.as_ref(), repo_name).await?;
    crate::registry::ensure_format(&repo, Format::Nuget)?;
    crate::registry::ensure_can_write(&*state.permissions, &repo, user).await?;
    crate::registry::ensure_hosted(&repo)?;
    Ok(repo)
}

enum Spool {
    Bytes(Bytes),
    TooLarge,
}

async fn spool(mut multipart: Multipart) -> AppResult<Spool> {
    let bad = |e: axum::extract::multipart::MultipartError| {
        AppError::BadRequest(format!("invalid multipart body: {e}"))
    };
    let mut field = multipart
        .next_field()
        .await
        .map_err(bad)?
        .ok_or_else(|| AppError::BadRequest("the push carries no package".to_string()))?;
    let mut buf = BytesMut::new();
    while let Some(chunk) = field.chunk().await.map_err(bad)? {
        if buf.len() + chunk.len() > MAX_NUPKG_BYTES {
            return Ok(Spool::TooLarge);
        }
        buf.extend_from_slice(&chunk);
    }
    Ok(Spool::Bytes(buf.freeze()))
}

/// PUT `/{repo}/v3/package`: 201 created, 400 invalid, 409 when the
/// normalized version exists, 413 over the cap.
pub async fn push(
    State(state): State<AppState>,
    Path(repo_name): Path<String>,
    auth: Option<axum::Extension<AuthUser>>,
    multipart: Multipart,
) -> AppResult<Response> {
    let user = require_user(auth)?;
    let repo = writable(&state, &repo_name, &user).await?;
    let body = match spool(multipart).await? {
        Spool::Bytes(b) => b,
        Spool::TooLarge => {
            return Ok((
                StatusCode::PAYLOAD_TOO_LARGE,
                Json(json!({"error": format!("a package is at most {MAX_NUPKG_BYTES} bytes")})),
            )
                .into_response())
        }
    };
    let parsing = body.clone();
    let (xml, parsed) = tokio::task::spawn_blocking(move || nuspec::from_package(&parsing))
        .await
        .map_err(|e| AppError::Internal(format!("nuspec task failed: {e}")))?
        .map_err(|e| AppError::BadRequest(e.to_string()))?;
    let version = NuGetVersion::parse(&parsed.version)
        .map_err(|e| AppError::BadRequest(e.to_string()))?;
    let facts = HostedFacts::new(
        parsed.clone(),
        String::from_utf8_lossy(&xml).into_owned(),
        &version,
    );
    let metadata_json = serde_json::to_string(&facts)?;
    let dependencies: Vec<NugetDependency<'_>> = parsed
        .dependency_groups
        .iter()
        .flat_map(|g| {
            g.dependencies.iter().map(move |d| NugetDependency {
                id: &d.id,
                range: d.range.as_deref().unwrap_or(""),
                framework: g.target_framework.as_deref().unwrap_or("any"),
            })
        })
        .collect();
    let pre_scan = state.publish_gate().run(Format::Nuget, &metadata_json).await?;
    let landed = PublishNugetPackage::new(
        state.packages.clone(),
        state.repos.clone(),
        state.deps.clone(),
        state.placer(),
        super::read::rules(),
    )
    .run(
        NugetPush {
            repository: repo.id,
            id: &parsed.id,
            version: &parsed.version,
            description: parsed.description.as_deref(),
            metadata_json: &metadata_json,
            dependencies: &dependencies,
            spool: body,
        },
        chrono::Utc::now(),
    )
    .await
    .map_err(|e| match AppError::from(e) {
        AppError::Conflict(_) => AppError::Conflict(format!(
            "{} {} already exists",
            parsed.id,
            version.key()
        )),
        other => other,
    })?;
    state
        .publish_tail()
        .run(
            &Published {
                format: Format::Nuget,
                repository: &repo.name,
                package: &landed.package.name,
                version: &landed.version.version,
                version_id: Some(landed.version.id),
                metadata_json: &metadata_json,
                published_by: &user.username,
            },
            pre_scan,
            chrono::Utc::now(),
        )
        .await;
    info!(package = %landed.package.name, version = %landed.version.version, repo = %repo.name, "NuGet package pushed");
    Ok(StatusCode::CREATED.into_response())
}

async fn set_listed(
    state: &AppState,
    repo_name: &str,
    id: &str,
    version: &str,
    auth: Option<axum::Extension<AuthUser>>,
    listed: bool,
) -> AppResult<()> {
    let user = require_user(auth)?;
    let id = id_of(id)?;
    let key = key_of(version)?;
    let repo = writable(state, repo_name, &user).await?;
    Yank::new(state.packages.clone())
        .run(repo.id, &id, &key, !listed)
        .await?;
    info!(package = %id, version = %key, repo = %repo.name, listed, "NuGet version listing changed");
    Ok(())
}

/// DELETE `/{repo}/v3/package/{id}/{version}`: an unlist, never a removal.
pub async fn unlist(
    State(state): State<AppState>,
    Path((repo_name, id, version)): Path<(String, String, String)>,
    auth: Option<axum::Extension<AuthUser>>,
) -> AppResult<StatusCode> {
    set_listed(&state, &repo_name, &id, &version, auth, false).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// POST `/{repo}/v3/package/{id}/{version}`: relist.
pub async fn relist(
    State(state): State<AppState>,
    Path((repo_name, id, version)): Path<(String, String, String)>,
    auth: Option<axum::Extension<AuthUser>>,
) -> AppResult<StatusCode> {
    set_listed(&state, &repo_name, &id, &version, auth, true).await?;
    Ok(StatusCode::OK)
}
