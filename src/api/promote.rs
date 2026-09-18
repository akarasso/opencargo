use std::collections::HashMap;

use axum::{
    extract::{Path, State},
    response::IntoResponse,
    Json,
};
use serde::Deserialize;
use serde_json::{json, Value};
use tracing::info;

use crate::app::promote::{is_conflict, PromoteVersion, Promoter, Request};
use crate::auth::middleware::AuthUser;
use crate::domain::{can_admin, RepoKind, Repository, Visibility};
use crate::error::{AppError, AppResult};
use crate::ports::packages::NameMatch;
use crate::registry::extract_package_name;
use crate::server::AppState;

// ---------------------------------------------------------------------------
// Request types
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct PromoteRequest {
    pub from: String,
    pub to: String,
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Rewrite the dist.tarball URL in version metadata to point to the target repo.
fn rewrite_tarball_url(
    metadata_json: &str,
    base_url: &str,
    from_repo: &str,
    to_repo: &str,
) -> String {
    let mut meta: Value = serde_json::from_str(metadata_json).unwrap_or(json!({}));

    if let Some(dist) = meta.get_mut("dist") {
        if let Some(obj) = dist.as_object_mut() {
            if let Some(tarball_val) = obj.get("tarball").cloned() {
                if let Some(tarball_url) = tarball_val.as_str() {
                    // Replace the repo name prefix in the tarball URL
                    let from_prefix = format!("{}/{}/", base_url, from_repo);
                    let to_prefix = format!("{}/{}/", base_url, to_repo);
                    let new_url = tarball_url.replacen(&from_prefix, &to_prefix, 1);
                    obj.insert("tarball".to_string(), Value::String(new_url));
                }
            }
        }
    }

    serde_json::to_string(&meta).unwrap_or_else(|_| metadata_json.to_string())
}

/// A path the target version owns: the layout is `{format}/{repo}/{...}`, so
/// only the repository segment changes.
fn target_path(source: &str, to_repo: &str) -> String {
    let parts: Vec<&str> = source.splitn(3, '/').collect();
    match parts.len() {
        3 => format!("{}/{to_repo}/{}", parts[0], parts[2]),
        _ => format!("{to_repo}/{source}"),
    }
}

/// The tags the source version holds; the promoted one inherits exactly
/// those, in the same transaction as the version itself.
async fn inherited_tags(
    state: &AppState,
    package: i64,
    version: i64,
) -> AppResult<Vec<String>> {
    Ok(state
        .packages
        .dist_tags(package)
        .await?
        .into_iter()
        .filter(|tag| tag.version_id == version)
        .map(|tag| tag.tag)
        .collect())
}

async fn refuse_existing(
    state: &AppState,
    to_repo: &Repository,
    name: &str,
    version: &str,
) -> AppResult<()> {
    let Some(package) = state
        .packages
        .package(to_repo.id, name, NameMatch::Exact)
        .await?
    else {
        return Ok(());
    };
    match state.packages.version(package.id, version).await? {
        Some(_) => Err(AppError::Conflict(format!(
            "version '{version}' already exists in repository '{}'",
            to_repo.name
        ))),
        None => Ok(()),
    }
}

/// Both repositories, refusing anything a promotion cannot move between: a
/// missing one, a non-hosted one, or a pair that does not share a format.
async fn hosted_pair(
    state: &AppState,
    body: &PromoteRequest,
) -> AppResult<(Repository, Repository)> {
    let from = state.repos.by_name(&body.from).await?.ok_or_else(|| {
        AppError::NotFound(format!("source repository not found: {}", body.from))
    })?;
    let to = state.repos.by_name(&body.to).await?.ok_or_else(|| {
        AppError::NotFound(format!("target repository not found: {}", body.to))
    })?;

    for (repo, side) in [(&from, "source"), (&to, "target")] {
        if repo.kind()? != RepoKind::Hosted {
            return Err(AppError::BadRequest(format!(
                "{side} repository '{}' is not a hosted repository",
                repo.name
            )));
        }
    }

    let (from_format, to_format) = (from.fmt()?, to.fmt()?);
    if from_format != to_format {
        return Err(AppError::BadRequest(format!(
            "cannot promote across formats: source is '{}', target is '{}'",
            from_format.as_str(),
            to_format.as_str()
        )));
    }
    Ok((from, to))
}

/// Steps 3 to 7: find the source version, refuse one the target already
/// holds, and hand the copy plus its metadata transaction to the use case.
async fn move_version(
    state: &AppState,
    body: &PromoteRequest,
    by: &AuthUser,
    repos: (&Repository, &Repository),
    what: (&str, &str),
) -> AppResult<()> {
    let (from_repo, to_repo) = repos;
    let (name, version) = what;

    let from_package = state
        .packages
        .package(from_repo.id, name, NameMatch::Exact)
        .await?
        .ok_or_else(|| {
            AppError::NotFound(format!(
                "package '{name}' not found in repository '{}'",
                from_repo.name
            ))
        })?;
    let from_version = state
        .packages
        .version(from_package.id, version)
        .await?
        .ok_or_else(|| {
            AppError::NotFound(format!(
                "version '{version}' of package '{name}' not found in repository '{}'",
                from_repo.name
            ))
        })?;

    // Refused before anything is copied; the store's own conflict is the
    // safety net for the race.
    refuse_existing(state, to_repo, name, version).await?;

    // The target version owns its artifact. Sharing the source path let the
    // cleanup GC, deleting the older pre-release source file, make the
    // promoted production artifact undownloadable — silent data loss.
    let target_tarball_path = target_path(&from_version.tarball_path, &to_repo.name);
    let metadata_json = rewrite_tarball_url(
        &from_version.metadata_json,
        &state.base_url,
        &body.from,
        &body.to,
    );
    let inherited = inherited_tags(state, from_package.id, from_version.id).await?;
    let details = json!({ "from": body.from, "to": body.to });

    PromoteVersion::new(state.packages.clone(), state.storage.clone())
        .run(
            Request {
                source: &from_version,
                target: to_repo,
                package: name,
                description: from_package.description.as_deref(),
                metadata_json: &metadata_json,
                target_path: &target_tarball_path,
                dist_tags: &inherited,
                details_json: &details.to_string(),
            },
            Promoter {
                user_id: by.user_id,
                username: &by.username,
            },
            chrono::Utc::now(),
        )
        .await
        .map_err(|err| {
            if is_conflict(&err) {
                AppError::Conflict(format!(
                    "version '{version}' already exists in repository '{}'",
                    to_repo.name
                ))
            } else {
                err.into()
            }
        })?;
    Ok(())
}

/// The webhook, the real-time event and the audit mirror, after the commit.
///
/// The source repository's name rides along only when that repository is
/// itself public: promoting private -> public must not disclose the staging
/// repository to anonymous or merely authenticated subscribers, who would
/// otherwise learn a name they have no read access to. Admins get it from the
/// audit entry.
async fn announce(
    state: &AppState,
    body: &PromoteRequest,
    by: &AuthUser,
    what: (&str, &str),
    target: &str,
) {
    let (name, version) = what;
    state
        .webhook_dispatcher
        .dispatch(
            "package.promoted",
            &json!({
                "package": name,
                "version": version,
                "from": body.from,
                "to": body.to,
                "promoted_by": by.username,
            }),
        )
        .await;

    let from_is_public = matches!(
        state.repos.by_name(&body.from).await,
        Ok(Some(ref r)) if r.visibility == Visibility::Public
    );
    let mut payload = json!({
        "package": name,
        "version": version,
        "to": body.to,
        "repository": body.to,
        "promoted_by": by.username,
    });
    if from_is_public {
        payload["from"] = json!(body.from);
    }
    crate::registry::emit_package_event(state, "package.promoted", &body.to, payload).await;

    // Mirrored on the bus so the admin audit view updates live: the entry
    // itself is written inside the promotion's transaction, not through
    // `record_audit`.
    state.events.emit(
        "audit.entry",
        crate::events::Visibility::Admin,
        json!({
            "username": by.username,
            "action": "package.promote",
            "target": target,
        }),
    );
}

// ---------------------------------------------------------------------------
// POST /api/v1/packages/@{scope}/{name}/versions/{version}/promote
// ---------------------------------------------------------------------------

pub async fn promote_package(
    State(state): State<AppState>,
    Path(params): Path<HashMap<String, String>>,
    request: axum::http::Request<axum::body::Body>,
) -> AppResult<impl IntoResponse> {
    let name = extract_package_name(&params);
    let version = params
        .get("version")
        .cloned()
        .ok_or_else(|| AppError::BadRequest("missing version".to_string()))?;

    promote_impl(state, name, version, request).await
}

/// POST /api/v1/packages/{name}/versions/{version}/promote (unscoped)
pub async fn promote_package_unscoped(
    State(state): State<AppState>,
    Path(params): Path<HashMap<String, String>>,
    request: axum::http::Request<axum::body::Body>,
) -> AppResult<impl IntoResponse> {
    let name = extract_package_name(&params);
    let version = params
        .get("version")
        .cloned()
        .ok_or_else(|| AppError::BadRequest("missing version".to_string()))?;

    promote_impl(state, name, version, request).await
}

async fn promote_impl(
    state: AppState,
    name: String,
    version: String,
    request: axum::http::Request<axum::body::Body>,
) -> AppResult<impl IntoResponse> {
    // 1. Require authentication + admin role
    let auth_user = request
        .extensions()
        .get::<AuthUser>()
        .cloned()
        .ok_or_else(|| AppError::Unauthorized("authentication required".to_string()))?;

    if !can_admin(&auth_user.role) {
        return Err(AppError::Forbidden("admin access required".to_string()));
    }

    // Parse the request body
    let body: PromoteRequest = {
        let bytes = axum::body::to_bytes(request.into_body(), 1024 * 1024)
            .await
            .map_err(|e| AppError::BadRequest(format!("failed to read body: {e}")))?;
        serde_json::from_slice(&bytes)?
    };

    // 2. Validate both repos exist and are hosted with the same format
    let (from_repo, to_repo) = hosted_pair(&state, &body).await?;

    let target_str = format!("{name}@{version}");
    move_version(&state, &body, &auth_user, (&from_repo, &to_repo), (&name, &version)).await?;

    announce(&state, &body, &auth_user, (&name, &version), &target_str).await;

    info!(
        package = %name,
        version = %version,
        from = %body.from,
        to = %body.to,
        "Package version promoted"
    );

    // 10. Return success
    Ok(Json(json!({
        "ok": true,
        "package": name,
        "version": version,
        "from": body.from,
        "to": body.to,
    })))
}

// ---------------------------------------------------------------------------
// GET /api/v1/packages/@{scope}/{name}/versions/{version}/promotions
// ---------------------------------------------------------------------------

pub async fn list_promotions(
    State(state): State<AppState>,
    Path(params): Path<HashMap<String, String>>,
    request: axum::http::Request<axum::body::Body>,
) -> AppResult<impl IntoResponse> {
    let name = extract_package_name(&params);
    let version = params
        .get("version")
        .cloned()
        .ok_or_else(|| AppError::BadRequest("missing version".to_string()))?;

    list_promotions_impl(state, name, version, request).await
}

/// GET /api/v1/packages/{name}/versions/{version}/promotions (unscoped)
pub async fn list_promotions_unscoped(
    State(state): State<AppState>,
    Path(params): Path<HashMap<String, String>>,
    request: axum::http::Request<axum::body::Body>,
) -> AppResult<impl IntoResponse> {
    let name = extract_package_name(&params);
    let version = params
        .get("version")
        .cloned()
        .ok_or_else(|| AppError::BadRequest("missing version".to_string()))?;

    list_promotions_impl(state, name, version, request).await
}

async fn list_promotions_impl(
    state: AppState,
    name: String,
    version: String,
    request: axum::http::Request<axum::body::Body>,
) -> AppResult<impl IntoResponse> {
    // Require authentication
    let _auth_user = request
        .extensions()
        .get::<AuthUser>()
        .cloned()
        .ok_or_else(|| AppError::Unauthorized("authentication required".to_string()))?;

    let target_str = format!("{}@{}", name, version);

    // Query the audit log for package.promote actions targeting this package+version
    let entries: Vec<crate::db::AuditEntry> = sqlx::query_as(
        "SELECT * FROM audit_log WHERE action = 'package.promote' AND target = ?1 ORDER BY created_at DESC",
    )
    .bind(&target_str)
    .fetch_all(&state.db)
    .await?;

    let promotions: Vec<Value> = entries
        .iter()
        .map(|e| {
            let details: Value = e
                .details_json
                .as_deref()
                .and_then(|d| serde_json::from_str(d).ok())
                .unwrap_or(json!({}));

            json!({
                "id": e.id,
                "package": name,
                "version": version,
                "from": details.get("from").and_then(|v| v.as_str()).unwrap_or(""),
                "to": details.get("to").and_then(|v| v.as_str()).unwrap_or(""),
                "promoted_by": e.username,
                "promoted_at": e.created_at,
            })
        })
        .collect();

    Ok(Json(json!({
        "package": name,
        "version": version,
        "promotions": promotions,
    })))
}
