//! `PackagePublish/2.0.0`: push, delete (= unlist) and relist.
//!
//! The push body is multipart and its first file field is the `.nupkg`,
//! spooled in memory under a cap: the spool is private to the request and is
//! what `place_shared` replays from when a commit is superseded. Every spool
//! draws its bytes from one process-wide budget, so concurrent pushes cannot
//! hold more than it; a push that cannot get its bytes in time is a 503.

use axum::{
    extract::{Multipart, Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use bytes::{Bytes, BytesMut};
use serde_json::json;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
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
const SPOOL_BUDGET_BYTES: usize = 512 * 1024 * 1024;
const SPOOL_WAIT: Duration = Duration::from_secs(30);

fn budget() -> Arc<Semaphore> {
    static BUDGET: OnceLock<Arc<Semaphore>> = OnceLock::new();
    BUDGET
        .get_or_init(|| Arc::new(Semaphore::new(SPOOL_BUDGET_BYTES)))
        .clone()
}

fn require_user(auth: Option<axum::Extension<AuthUser>>) -> AppResult<AuthUser> {
    auth.map(|e| e.0)
        .ok_or_else(|| AppError::Unauthorized("authentication required".to_string()))
}

/// The hosted NuGet repository `repo_name` names; the right is judged once
/// the package id is known.
async fn hosted_nuget(state: &AppState, repo_name: &str) -> AppResult<Repository> {
    let repo = crate::registry::load_repo(state.repos.as_ref(), repo_name).await?;
    crate::registry::ensure_format(&repo, Format::Nuget)?;
    crate::registry::ensure_hosted(&repo)?;
    Ok(repo)
}

/// The package's bytes and their share of the budget, returned on drop.
struct Spooled {
    bytes: Bytes,
    _budget: Option<OwnedSemaphorePermit>,
}

enum Spool {
    Bytes(Spooled),
    TooLarge,
    Busy,
}

struct Limits {
    cap: usize,
    budget: Arc<Semaphore>,
    wait: Duration,
}

async fn spool(mut multipart: Multipart, limits: &Limits) -> AppResult<Spool> {
    let bad = |e: axum::extract::multipart::MultipartError| {
        AppError::BadRequest(format!("invalid multipart body: {e}"))
    };
    let mut field = multipart
        .next_field()
        .await
        .map_err(bad)?
        .ok_or_else(|| AppError::BadRequest("the push carries no package".to_string()))?;
    let deadline = tokio::time::Instant::now() + limits.wait;
    let mut held: Option<OwnedSemaphorePermit> = None;
    let mut buf = BytesMut::new();
    while let Some(chunk) = field.chunk().await.map_err(bad)? {
        if buf.len() + chunk.len() > limits.cap {
            return Ok(Spool::TooLarge);
        }
        let want = u32::try_from(chunk.len()).unwrap_or(u32::MAX);
        if want > 0 {
            let got = tokio::time::timeout_at(deadline, limits.budget.clone().acquire_many_owned(want)).await;
            match got {
                Ok(Ok(permit)) => match held.as_mut() {
                    Some(h) => h.merge(permit),
                    None => held = Some(permit),
                },
                Ok(Err(_)) => return Err(AppError::Internal("spool budget closed".to_string())),
                Err(_) => return Ok(Spool::Busy),
            }
        }
        buf.extend_from_slice(&chunk);
    }
    Ok(Spool::Bytes(Spooled {
        bytes: buf.freeze(),
        _budget: held,
    }))
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
    crate::registry::meter_publish(&state, &user, Format::Nuget, &repo_name)?;
    let repo = hosted_nuget(&state, &repo_name).await?;
    let limits = Limits {
        cap: MAX_NUPKG_BYTES,
        budget: budget(),
        wait: SPOOL_WAIT,
    };
    let spooled = match spool(multipart, &limits).await? {
        Spool::Bytes(s) => s,
        Spool::TooLarge => {
            return Ok((
                StatusCode::PAYLOAD_TOO_LARGE,
                Json(json!({"error": format!("a package is at most {MAX_NUPKG_BYTES} bytes")})),
            )
                .into_response())
        }
        Spool::Busy => {
            return Err(AppError::ServiceUnavailable(
                "too many pushes in flight, retry later".to_string(),
            ))
        }
    };
    let body = spooled.bytes.clone();
    let parsing = body.clone();
    let (xml, parsed) = tokio::task::spawn_blocking(move || nuspec::from_package(&parsing))
        .await
        .map_err(|e| AppError::Internal(format!("nuspec task failed: {e}")))?
        .map_err(|e| AppError::BadRequest(e.to_string()))?;
    crate::registry::ensure_can_write(&state.authorize(), &repo, Some(&parsed.id), &user).await?;
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
    let repo = hosted_nuget(state, repo_name).await?;
    crate::registry::ensure_can_write(&state.authorize(), &repo, Some(&id), &user).await?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use axum::extract::FromRequest;

    fn multipart(len: usize) -> Multipart {
        let boundary = "b0undary";
        let mut body = format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"package\"; filename=\"p.nupkg\"\r\nContent-Type: application/octet-stream\r\n\r\n"
        )
        .into_bytes();
        body.extend(std::iter::repeat_n(b'x', len));
        body.extend(format!("\r\n--{boundary}--\r\n").into_bytes());
        let req = axum::http::Request::builder()
            .header("content-type", format!("multipart/form-data; boundary={boundary}"))
            .body(axum::body::Body::from(body))
            .unwrap();
        futures_util::FutureExt::now_or_never(Multipart::from_request(req, &()))
            .expect("ready")
            .expect("multipart")
    }

    fn limits(cap: usize, budget: usize) -> Limits {
        Limits {
            cap,
            budget: Arc::new(Semaphore::new(budget)),
            wait: Duration::from_secs(30),
        }
    }

    #[tokio::test]
    async fn a_field_over_the_cap_is_too_large_and_holds_nothing() {
        let l = limits(1000, 4000);
        assert!(matches!(spool(multipart(1001), &l).await.unwrap(), Spool::TooLarge));
        assert_eq!(l.budget.available_permits(), 4000, "the budget is whole again");
    }

    #[tokio::test]
    async fn a_spool_holds_its_bytes_of_the_budget_until_dropped() {
        let l = limits(1000, 4000);
        let Spool::Bytes(held) = spool(multipart(900), &l).await.unwrap() else {
            panic!("spooled")
        };
        assert_eq!(held.bytes.len(), 900);
        assert_eq!(l.budget.available_permits(), 3100);
        drop(held);
        assert_eq!(l.budget.available_permits(), 4000);
    }

    #[tokio::test(start_paused = true)]
    async fn concurrent_spools_past_the_budget_wait_then_are_busy() {
        let l = limits(1000, 1500);
        let Spool::Bytes(first) = spool(multipart(900), &l).await.unwrap() else {
            panic!("spooled")
        };
        assert!(matches!(spool(multipart(900), &l).await.unwrap(), Spool::Busy));
        drop(first);
        assert!(matches!(spool(multipart(900), &l).await.unwrap(), Spool::Bytes(_)));
    }
}
