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

use crate::app::publish::Artifact;
use crate::app::releases::Yank;
use crate::app::publish_tail::Published;
use crate::auth::middleware::AuthUser;
use crate::domain::{Format, Repository};
use crate::error::{AppError, AppResult};
use crate::ports::packages::NameMatch;
use crate::server::AppState;

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

/// The crate's `cksum`, off the runtime's threads: a crate is up to the
/// body cap and hashing it inline would stall the executor.
async fn checksum(data: &Bytes) -> AppResult<String> {
    let data = data.clone();
    tokio::task::spawn_blocking(move || hex_encode(sha2::Sha256::digest(&data)))
        .await
        .map_err(|e| AppError::Internal(format!("checksum task failed: {e}")))
}

pub async fn publish_crate(
    State(state): State<AppState>,
    Path(repo_name): Path<String>,
    auth_user: Option<axum::Extension<AuthUser>>,
    body: Bytes,
) -> AppResult<impl IntoResponse> {
    let user = require_user(auth_user)?;
    crate::registry::meter_publish(&state, &user, Format::Cargo, &repo_name)?;
    let repo = load_hosted(&state, &repo_name, &user).await?;
    let (meta, crate_data) = parse_publish_body(&body)?;
    crate::registry::rules::rules_of(crate::domain::Format::Cargo)?.validate(&meta.name)?;
    crate::registry::rules::rules_of(crate::domain::Format::Cargo)?.validate_version(&meta.vers)?;

    let sha256_hex = checksum(&crate_data).await?;
    let metadata_json = serde_json::to_string(&meta)?;
    let pre_scan = state
        .publish_gate()
        .run(Format::Cargo, &metadata_json)
        .await?;

    // Read before write, as before: the store's own conflict is the safety
    // net for the race, but without this a duplicate publish would overwrite
    // the stored artifact of the version it is about to be refused for.
    refuse_duplicate(&state, repo.id, &meta).await?;

    let filename = format!("{}-{}.crate", meta.name, meta.vers);
    let size = crate_data.len() as i64;
    let landed = state.publish_version()
        .run(
            Artifact {
                repository: repo.id,
                package: &meta.name,
                // Crate names are unique whatever case a client sends.
                match_name: NameMatch::Insensitive,
                description: meta.description.as_deref(),
                readme: None,
                version: &meta.vers,
                metadata_json: &metadata_json,
                checksum_sha1: None,
                checksum_sha256: Some(&sha256_hex),
                integrity: None,
                filename: &filename,
                dist_tags: &[],
                bytes: crate_data,
            },
            chrono::Utc::now(),
        )
        .await
        .map_err(|err| duplicate_named(err, &meta.name, &meta.vers))?;
    let version_id = landed.version.id;
    record_dependencies(&state, landed.package.id, version_id, &meta.deps).await;
    state
        .publish_tail()
        .run(
            &Published {
                format: Format::Cargo,
                repository: &repo_name,
                package: &meta.name,
                version: &meta.vers,
                version_id: Some(version_id),
                metadata_json: &metadata_json,
                published_by: &user.username,
            },
            pre_scan,
            chrono::Utc::now(),
        )
        .await;

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
    Yank::new(state.packages.clone())
        .run(repo.id, name, version, yanked)
        .await?;
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
    let repo = crate::registry::load_repo(state.repos.as_ref(), repo_name).await?;
    crate::registry::ensure_hosted(&repo)?;
    crate::registry::ensure_format(&repo, Format::Cargo)?;
    crate::registry::ensure_can_write(&*state.permissions, &repo, user).await?;
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

async fn refuse_duplicate(
    state: &AppState,
    repo_id: i64,
    meta: &CargoPublishMeta,
) -> AppResult<()> {
    let Some(package) = state
        .packages
        .package(repo_id, &meta.name, NameMatch::Insensitive)
        .await?
    else {
        return Ok(());
    };
    match state.packages.version(package.id, &meta.vers).await? {
        Some(_) => Err(AppError::Conflict(format!(
            "version {} already exists for {}",
            meta.vers, meta.name
        ))),
        None => Ok(()),
    }
}

/// The store's bare conflict, back in cargo's words: the pre-insert read this
/// replaced named the crate and the version it refused.
fn duplicate_named(
    err: crate::app::publish::PublishError,
    name: &str,
    version: &str,
) -> AppError {
    use crate::app::publish::PublishError;
    use crate::error::StoreError;
    match err {
        PublishError::Store(StoreError::Conflict) => AppError::Conflict(format!(
            "version {version} already exists for {name}"
        )),
        other => other.into(),
    }
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
        let dep = crate::ports::deps::NewDependency {
            package: package_id,
            version: version_id,
            name,
            requirement: req,
            kind,
        };
        if let Err(e) = state.deps.record(&dep, chrono::Utc::now()).await {
            tracing::warn!(dependency = %name, "failed to record dependency (graph may be incomplete): {e}");
        }
    }
}

fn hex_encode(bytes: impl AsRef<[u8]>) -> String {
    bytes.as_ref().iter().map(|b| format!("{b:02x}")).collect()
}
