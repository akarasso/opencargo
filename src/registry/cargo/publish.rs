use std::collections::HashMap;

use axum::{
    body::Bytes,
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
    Json,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::Digest;
use tracing::info;

use crate::auth::middleware::AuthUser;
use crate::db::kinds::Format;
use crate::db::{Package, Repository};
use crate::error::{AppError, AppResult};
use crate::server::AppState;
use crate::storage::StorageBackend;

/// Cargo publish metadata (the JSON portion of the PUT body).
#[derive(Debug, Deserialize, Serialize)]
struct CargoPublishMeta {
    name: String,
    vers: String,
    #[serde(default)]
    deps: Value,
    #[serde(default)]
    features: Value,
    #[serde(default)]
    authors: Vec<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    license: Option<String>,
    #[serde(default)]
    readme: Option<String>,
    #[serde(default)]
    repository: Option<String>,
    #[serde(default)]
    links: Option<String>,
    #[serde(default)]
    features2: Option<Value>,
    #[serde(flatten)]
    extra: HashMap<String, Value>,
}

pub async fn publish_crate(
    State(state): State<AppState>,
    Path(repo_name): Path<String>,
    auth_user: Option<axum::Extension<AuthUser>>,
    body: Bytes,
) -> AppResult<impl IntoResponse> {
    let user = require_user(auth_user)?;
    let repo = load_hosted(&state, &repo_name, &user).await?;
    let (meta, crate_data) = parse_publish_body(&body)?;
    crate::registry::validate_package_name("cargo", &meta.name)?;
    crate::registry::validate_version(&meta.vers)?;

    let sha256_hex = {
        let data = crate_data.clone();
        tokio::task::spawn_blocking(move || hex_encode(sha2::Sha256::digest(&data)))
            .await
            .map_err(|e| AppError::Internal(format!("checksum task failed: {e}")))?
    };
    let metadata_json = serde_json::to_string(&meta)?;
    let pre_scan =
        crate::registry::publish::publish_gate(&state, Format::Cargo, &metadata_json).await?;

    let package = get_or_create_package(&state, repo.id, &meta).await?;
    if crate::db::get_version(&state.db, package.id, &meta.vers)
        .await?
        .is_some()
    {
        return Err(AppError::Conflict(format!(
            "version {} already exists for {}",
            meta.vers, meta.name
        )));
    }
    let storage_path = format!(
        "cargo/{repo_name}/{}/{}-{}.crate",
        meta.name, meta.name, meta.vers
    );
    state.storage.put(&storage_path, crate_data.clone()).await?;
    let size = crate_data.len() as i64;
    let version_id = crate::db::create_version(
        &state.db,
        package.id,
        &meta.vers,
        &metadata_json,
        None,
        Some(&sha256_hex),
        None,
        size,
        &storage_path,
    )
    .await?;
    record_dependencies(&state, package.id, version_id, &meta.deps).await;
    crate::registry::publish::finalize_publish(
        &state,
        Format::Cargo,
        &repo_name,
        &meta.name,
        &meta.vers,
        Some(version_id),
        &metadata_json,
        &user.username,
        pre_scan,
    )
    .await?;

    info!(crate_name = %meta.name, version = %meta.vers, size, repo = %repo_name, "Cargo crate published");
    Ok((
        StatusCode::OK,
        Json(json!({
            "warnings": { "invalid_categories": [], "invalid_badges": [], "other": [] }
        })),
    ))
}

pub async fn yank(
    State(state): State<AppState>,
    Path((repo_name, name, version)): Path<(String, String, String)>,
    auth_user: Option<axum::Extension<AuthUser>>,
) -> AppResult<impl IntoResponse> {
    set_yanked(&state, &repo_name, &name, &version, auth_user, true).await
}

pub async fn unyank(
    State(state): State<AppState>,
    Path((repo_name, name, version)): Path<(String, String, String)>,
    auth_user: Option<axum::Extension<AuthUser>>,
) -> AppResult<impl IntoResponse> {
    set_yanked(&state, &repo_name, &name, &version, auth_user, false).await
}

async fn set_yanked(
    state: &AppState,
    repo_name: &str,
    name: &str,
    version: &str,
    auth_user: Option<axum::Extension<AuthUser>>,
    yanked: bool,
) -> AppResult<Json<Value>> {
    let user = require_user(auth_user)?;
    let repo = load_hosted(state, repo_name, &user).await?;
    let package = crate::db::get_package(&state.db, repo.id, name)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("crate not found: {name}")))?;
    let row = crate::db::get_version(&state.db, package.id, version)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("version not found: {name}@{version}")))?;
    crate::db::set_yanked(&state.db, row.id, yanked).await?;
    let action = if yanked { "yanked" } else { "unyanked" };
    info!(crate_name = %name, version = %version, repo = %repo_name, "Cargo crate version {action}");
    Ok(Json(json!({"ok": true})))
}

fn require_user(auth_user: Option<axum::Extension<AuthUser>>) -> AppResult<AuthUser> {
    auth_user
        .map(|e| e.0)
        .ok_or_else(|| AppError::Unauthorized("authentication required".to_string()))
}

/// Writes go to hosted cargo repositories the caller may write to.
async fn load_hosted(state: &AppState, repo_name: &str, user: &AuthUser) -> AppResult<Repository> {
    let repo = crate::registry::load_repo(&state.db, repo_name).await?;
    crate::registry::ensure_hosted(&repo)?;
    crate::registry::ensure_format(&repo, Format::Cargo)?;
    crate::registry::ensure_can_write(&state.db, &repo, user).await?;
    Ok(repo)
}

/// The binary publish format: LE u32 JSON length, JSON metadata, LE u32
/// crate length, the `.crate` bytes (a zero-copy slice of the body).
fn parse_publish_body(body: &Bytes) -> AppResult<(CargoPublishMeta, Bytes)> {
    let short = |what: &str| AppError::BadRequest(format!("request body too short: {what}"));
    let data = body.as_ref();
    let json_len = read_len(data, 0).ok_or_else(|| short("missing JSON length"))?;
    let json_end = 4 + json_len;
    let crate_len = read_len(data, json_end).ok_or_else(|| short("missing crate data"))?;
    let crate_start = json_end + 4;
    let crate_end = crate_start + crate_len;
    if data.len() < crate_end {
        return Err(short("crate file truncated"));
    }
    let meta: CargoPublishMeta = serde_json::from_slice(&data[4..json_end])
        .map_err(|e| AppError::BadRequest(format!("invalid publish metadata JSON: {e}")))?;
    Ok((meta, body.slice(crate_start..crate_end)))
}

fn read_len(data: &[u8], at: usize) -> Option<usize> {
    let bytes = data.get(at..at + 4)?;
    Some(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as usize)
}

async fn get_or_create_package(
    state: &AppState,
    repo_id: i64,
    meta: &CargoPublishMeta,
) -> AppResult<Package> {
    if let Some(p) = crate::db::get_package(&state.db, repo_id, &meta.name).await? {
        return Ok(p);
    }
    crate::db::create_package(&state.db, repo_id, &meta.name, meta.description.as_deref()).await?;
    crate::db::get_package(&state.db, repo_id, &meta.name)
        .await?
        .ok_or_else(|| AppError::Internal(format!("failed to create package: {}", meta.name)))
}

async fn record_dependencies(state: &AppState, package_id: i64, version_id: i64, deps: &Value) {
    for dep in deps.as_array().into_iter().flatten() {
        let name = dep.get("name").and_then(Value::as_str).unwrap_or("");
        if name.is_empty() {
            continue;
        }
        let req = dep
            .get("version_req")
            .and_then(Value::as_str)
            .unwrap_or("*");
        let kind = match dep.get("kind").and_then(Value::as_str) {
            Some("dev") => "dev",
            Some("build") => "build",
            _ => "normal",
        };
        if let Err(e) =
            crate::db::insert_dependency(&state.db, package_id, version_id, name, req, kind).await
        {
            tracing::warn!(dependency = %name, "failed to record dependency (graph may be incomplete): {e}");
        }
    }
}

fn hex_encode(bytes: impl AsRef<[u8]>) -> String {
    bytes.as_ref().iter().map(|b| format!("{b:02x}")).collect()
}
