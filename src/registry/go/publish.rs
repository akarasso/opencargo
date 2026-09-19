use std::collections::HashMap;

use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
    Json,
};
use bytes::Bytes;
use serde_json::json;
use tracing::info;

use crate::app::publish::Artifact;
use crate::app::publish_tail::Published;
use crate::auth::middleware::AuthUser;
use crate::domain::{Format, Repository};
use crate::error::{AppError, AppResult};
use crate::ports::packages::NameMatch;
use crate::server::AppState;

const MAX_BODY_BYTES: usize = 100 * 1024 * 1024;
const MAX_GO_MOD_BYTES: u64 = 1024 * 1024;

fn param<'a>(params: &'a HashMap<String, String>, key: &str) -> AppResult<&'a str> {
    params
        .get(key)
        .map(String::as_str)
        .ok_or_else(|| AppError::BadRequest(format!("missing {key}")))
}

/// PUT /{repo}/{module}/@v/{version}: hosted only, the raw (unescaped)
/// module path, validated before any DB or storage access.
pub async fn publish_module(
    State(state): State<AppState>,
    Path(params): Path<HashMap<String, String>>,
    request: axum::http::Request<axum::body::Body>,
) -> AppResult<impl IntoResponse> {
    let auth_user = request
        .extensions()
        .get::<AuthUser>()
        .cloned()
        .ok_or_else(|| AppError::Unauthorized("authentication required".to_string()))?;
    let repo_name = param(&params, "repo")?;
    crate::registry::meter_publish(&state, &auth_user, Format::Go, repo_name)?;
    let module_name = param(&params, "module")?;
    let version_str = param(&params, "version")?;
    crate::registry::rules::rules_of(crate::domain::Format::Go)?.validate(module_name)?;
    crate::registry::rules::rules_of(crate::domain::Format::Go)?.validate_version(version_str)?;

    let repo = crate::registry::load_repo(state.repos.as_ref(), repo_name).await?;
    crate::registry::ensure_can_write(&state.authorize(), &repo, &auth_user).await?;
    crate::registry::ensure_hosted(&repo)?;
    crate::registry::ensure_format(&repo, Format::Go)?;

    let zip_data = axum::body::to_bytes(request.into_body(), MAX_BODY_BYTES)
        .await
        .map_err(|e| AppError::BadRequest(format!("failed to read body: {e}")))?;
    let go_mod_content = {
        let zip_data = zip_data.clone();
        let module_name = module_name.to_string();
        tokio::task::spawn_blocking(move || extract_go_mod_from_zip(&zip_data, &module_name))
            .await
            .map_err(|e| AppError::Internal(format!("go.mod extraction task failed: {e}")))??
    };
    let pre_scan = state.publish_gate().run(Format::Go, &go_mod_content).await?;

    let version_id = store_version(
        &state,
        &repo,
        module_name,
        version_str,
        zip_data,
        &go_mod_content,
    )
    .await?;
    state
        .publish_tail()
        .run(
            &Published {
                format: Format::Go,
                repository: repo_name,
                package: module_name,
                version: version_str,
                version_id: Some(version_id),
                metadata_json: &go_mod_content,
                published_by: &auth_user.username,
            },
            pre_scan,
            chrono::Utc::now(),
        )
        .await;

    info!(module = %module_name, version = %version_str, repo = %repo_name, "Go module published");
    Ok((StatusCode::OK, Json(json!({"ok": true}))))
}

/// The package row, the zip under `go/{repo}/{module}/{version}.zip` and the
/// version row carrying go.mod as its metadata.
async fn store_version(
    state: &AppState,
    repo: &Repository,
    module_name: &str,
    version_str: &str,
    zip_data: Bytes,
    go_mod_content: &str,
) -> AppResult<i64> {
    // Read before write: the store's conflict is the safety net for the race,
    // but without this a duplicate publish would overwrite the zip of the
    // version it is about to be refused for.
    if let Some(package) = state
        .packages
        .package(repo.id, module_name, NameMatch::Exact)
        .await?
    {
        if state
            .packages
            .version(package.id, version_str)
            .await?
            .is_some()
        {
            return Err(AppError::Conflict(format!(
                "version {version_str} already exists for {module_name}"
            )));
        }
    }

    let description = format!("Go module {module_name}");
    let filename = format!("{version_str}.zip");
    let landed = state.publish_version()
        .run(
            Artifact {
                repository: repo.id,
                package: module_name,
                match_name: NameMatch::Exact,
                description: Some(&description),
                readme: None,
                version: version_str,
                metadata_json: go_mod_content,
                checksum_sha1: None,
                checksum_sha256: None,
                integrity: None,
                filename: &filename,
                dist_tags: &[],
                bytes: zip_data,
            },
            chrono::Utc::now(),
        )
        .await?;
    Ok(landed.version.id)
}

/// The first `go.mod` in the archive (`{module}@{version}/go.mod` by
/// convention), or a minimal one; the inflated read is capped against zip bombs.
fn extract_go_mod_from_zip(zip_data: &[u8], module_name: &str) -> AppResult<String> {
    use crate::registry::archive::{zip_check, zip_member, ArchiveError, Budget, Limits};
    let refused = |e: ArchiveError| match e {
        ArchiveError::Unreadable(why) => AppError::BadRequest(format!("invalid zip file: {why}")),
        ArchiveError::TooLarge(..) => AppError::BadRequest("go.mod entry too large".to_string()),
        other => AppError::BadRequest(format!("invalid zip file: {other}")),
    };
    let every = Limits {
        max_members: None,
        max_inflated: None,
    };
    zip_check(zip_data, every).map_err(refused)?;
    let budget = Budget {
        max_members: usize::MAX,
        max_member_bytes: MAX_GO_MOD_BYTES,
    };
    match zip_member(zip_data, budget, |n| n.ends_with("go.mod")).map_err(refused)? {
        Some((_, bytes)) => String::from_utf8(bytes)
            .map_err(|e| AppError::BadRequest(format!("failed to read go.mod: {e}"))),
        None => Ok(format!("module {module_name}\n\ngo 1.21\n")),
    }
}
