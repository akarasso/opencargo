use std::collections::HashMap;

use axum::{
    body::Bytes,
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
    Json,
};
use base64::Engine;
use serde::Deserialize;
use serde_json::{json, Value};
use sha1::Digest;
use sqlx::SqlitePool;
use tracing::info;

use crate::auth::middleware::AuthUser;
use crate::domain::{Format, Package, Repository, Version};
use crate::error::{AppError, AppResult};
use crate::registry::extract_package_name;
use crate::registry::publish::{finalize_publish, publish_gate, PreScan};
use crate::server::AppState;

const MAX_README_BYTES: usize = 256 * 1024;

#[derive(Deserialize)]
pub struct PublishBody {
    name: String,
    description: Option<String>,
    #[serde(rename = "dist-tags", default)]
    dist_tags: HashMap<String, String>,
    #[serde(default)]
    versions: HashMap<String, Value>,
    #[serde(rename = "_attachments", default)]
    attachments: HashMap<String, Attachment>,
    /// Absent on metadata-only PUTs (deprecate), which must not clobber it.
    #[serde(default)]
    readme: Option<String>,
}

#[derive(Deserialize)]
pub struct Attachment {
    data: String,
}

/// One version of the publish body, decided before any write.
enum Step {
    Update { existing: Version, meta: Value },
    New(NewVersion),
}

struct NewVersion {
    version: String,
    tarball: Bytes,
    sha1: String,
    sha256: String,
    integrity: String,
    storage_path: String,
    meta: Value,
    metadata_json: String,
    pre: PreScan,
}

pub async fn publish_package(
    State(state): State<AppState>,
    Path(params): Path<HashMap<String, String>>,
    request: axum::http::Request<axum::body::Body>,
) -> AppResult<impl IntoResponse> {
    let auth_user = request
        .extensions()
        .get::<AuthUser>()
        .cloned()
        .ok_or_else(|| AppError::Unauthorized("authentication required".to_string()))?;

    let rate_key = format!("publish:{}", auth_user.username);
    if !state.publish_rate_limiter.check(&rate_key) {
        return Err(AppError::TooManyRequests(
            "too many publish requests, try again later".to_string(),
        ));
    }

    let body: PublishBody = {
        let bytes = axum::body::to_bytes(request.into_body(), 100 * 1024 * 1024)
            .await
            .map_err(|e| AppError::BadRequest(format!("failed to read body: {e}")))?;
        serde_json::from_slice(&bytes)?
    };

    let repo_name = super::param(&params, "repo")?;
    let package_name = extract_package_name(&params);
    crate::domain::validate_package_name("npm", &package_name)?;
    if body.name != package_name {
        return Err(AppError::BadRequest(format!(
            "package name in body ('{}') does not match URL ('{}')",
            body.name, package_name
        )));
    }

    let repo = crate::registry::load_repo(&state.db, repo_name).await?;
    crate::registry::ensure_hosted(&repo)?;
    crate::registry::ensure_format(&repo, Format::Npm)?;
    crate::registry::ensure_can_write(&state.db, &repo, &auth_user).await?;

    let steps = plan_versions(&state, &repo, &package_name, &body).await?;

    let package =
        get_or_create_package(&state.db, repo.id, &package_name, body.description.as_deref())
            .await?;
    store_readme(&state.db, package.id, body.readme.as_deref()).await?;
    for step in steps {
        match step {
            Step::Update { existing, meta } => {
                apply_metadata_update(&state.db, &existing, &meta).await?
            }
            Step::New(version) => {
                store_version(&state, &repo, &package, &body.dist_tags, version, &auth_user)
                    .await?
            }
        }
    }

    Ok((StatusCode::OK, Json(json!({"ok": true}))))
}

/// Validate, decode, checksum and gate every version before the first write:
/// a refused publish leaves neither file nor row.
async fn plan_versions(
    state: &AppState,
    repo: &Repository,
    package_name: &str,
    body: &PublishBody,
) -> AppResult<Vec<Step>> {
    let existing = crate::db::get_package(&state.db, repo.id, package_name).await?;
    let mut steps = Vec::with_capacity(body.versions.len());
    for (version_str, version_meta) in &body.versions {
        crate::domain::validate_version(version_str)?;

        // A publish carries a tarball attachment; `npm deprecate` does not.
        let attachment = find_attachment_key(&body.attachments, package_name, version_str)
            .and_then(|k| body.attachments.get(&k));

        let existing_version = match &existing {
            Some(package) => crate::db::get_version(&state.db, package.id, version_str).await?,
            None => None,
        };
        if let Some(existing) = existing_version {
            if attachment.is_some() {
                return Err(AppError::Conflict(format!(
                    "version {version_str} already exists for {package_name}"
                )));
            }
            steps.push(Step::Update {
                existing,
                meta: version_meta.clone(),
            });
            continue;
        }

        let attachment = attachment.ok_or_else(|| {
            AppError::BadRequest(format!("no attachment found for version {version_str}"))
        })?;
        let prepared =
            prepare_version(state, &repo.name, package_name, version_str, version_meta, attachment)
                .await?;
        steps.push(Step::New(prepared));
    }
    Ok(steps)
}

async fn prepare_version(
    state: &AppState,
    repo_name: &str,
    package_name: &str,
    version_str: &str,
    version_meta: &Value,
    attachment: &Attachment,
) -> AppResult<NewVersion> {
    let tarball = Bytes::from(
        base64::engine::general_purpose::STANDARD
            .decode(&attachment.data)
            .map_err(|e| AppError::BadRequest(format!("invalid base64 attachment: {e}")))?,
    );
    let (sha1, sha256, integrity) = checksums(tarball.clone()).await?;

    if let Some(expected) = version_meta
        .get("dist")
        .and_then(|d| d.get("shasum"))
        .and_then(|v| v.as_str())
    {
        if !expected.is_empty() && expected != sha1 {
            return Err(AppError::BadRequest(format!(
                "shasum mismatch: expected {expected}, got {sha1}"
            )));
        }
    }

    let tarball_filename = build_tarball_filename(package_name, version_str);
    let storage_path = format!("npm/{repo_name}/{package_name}/{tarball_filename}");
    let tarball_url = format!(
        "{}/{repo_name}/{package_name}/-/{tarball_filename}",
        state.base_url
    );
    let meta = with_dist(version_meta, tarball_url, &sha1, &integrity);
    let metadata_json = serde_json::to_string(&meta)?;
    let pre = publish_gate(state, Format::Npm, &metadata_json).await?;

    Ok(NewVersion {
        version: version_str.to_string(),
        tarball,
        sha1,
        sha256,
        integrity,
        storage_path,
        meta,
        metadata_json,
        pre,
    })
}

/// Three digest passes over a tarball of up to 100 MiB, off the async workers.
async fn checksums(data: Bytes) -> AppResult<(String, String, String)> {
    tokio::task::spawn_blocking(move || {
        let sha1 = hex(sha1::Sha1::digest(&data));
        let sha256 = hex(sha2::Sha256::digest(&data));
        let integrity = format!(
            "sha512-{}",
            base64::engine::general_purpose::STANDARD.encode(sha2::Sha512::digest(&data))
        );
        (sha1, sha256, integrity)
    })
    .await
    .map_err(|e| AppError::Internal(format!("checksum task failed: {e}")))
}

/// Point `dist` at this server with the checksums we computed.
fn with_dist(version_meta: &Value, tarball_url: String, sha1: &str, integrity: &str) -> Value {
    let mut meta = version_meta.clone();
    let dist = json!({
        "tarball": tarball_url,
        "shasum": sha1,
        "integrity": integrity,
    });
    match meta.get_mut("dist").and_then(|d| d.as_object_mut()) {
        Some(existing) => existing.extend(dist.as_object().cloned().unwrap_or_default()),
        None => {
            if let Some(obj) = meta.as_object_mut() {
                obj.insert("dist".to_string(), dist);
            }
        }
    }
    meta
}

async fn store_version(
    state: &AppState,
    repo: &Repository,
    package: &Package,
    dist_tags: &HashMap<String, String>,
    v: NewVersion,
    auth_user: &AuthUser,
) -> AppResult<()> {
    let size = v.tarball.len() as i64;
    state.storage.put(&v.storage_path, v.tarball).await?;

    let version_id = crate::db::create_version(
        &state.db,
        package.id,
        &v.version,
        &v.metadata_json,
        Some(&v.sha1),
        Some(&v.sha256),
        Some(&v.integrity),
        size,
        &v.storage_path,
    )
    .await?;

    for (tag, tag_version) in dist_tags {
        if tag_version == &v.version {
            crate::db::set_dist_tag(&state.db, package.id, tag, version_id).await?;
        }
    }
    record_dependencies(&state.db, package.id, version_id, &v.meta).await;

    finalize_publish(
        state,
        Format::Npm,
        &repo.name,
        &package.name,
        &v.version,
        Some(version_id),
        &v.metadata_json,
        &auth_user.username,
        v.pre,
    )
    .await?;

    info!(
        package = %package.name,
        version = %v.version,
        size,
        repo = %repo.name,
        "Package version published"
    );
    Ok(())
}

async fn record_dependencies(db: &SqlitePool, package_id: i64, version_id: i64, meta: &Value) {
    const DEP_TYPES: [(&str, &str); 4] = [
        ("dependencies", "runtime"),
        ("devDependencies", "dev"),
        ("peerDependencies", "peer"),
        ("optionalDependencies", "optional"),
    ];
    for (field, dep_type) in DEP_TYPES {
        let Some(deps) = meta.get(field).and_then(|v| v.as_object()) else {
            continue;
        };
        for (dep_name, dep_version) in deps {
            let version_req = dep_version.as_str().unwrap_or("*");
            let recorded = crate::db::insert_dependency(
                db, package_id, version_id, dep_name, version_req, dep_type,
            )
            .await;
            if let Err(e) = recorded {
                tracing::warn!(
                    dependency = %dep_name,
                    "failed to record dependency (graph may be incomplete): {e}"
                );
            }
        }
    }
}

async fn get_or_create_package(
    db: &SqlitePool,
    repo_id: i64,
    package_name: &str,
    description: Option<&str>,
) -> AppResult<Package> {
    if let Some(package) = crate::db::get_package(db, repo_id, package_name).await? {
        return Ok(package);
    }
    crate::db::create_package(db, repo_id, package_name, description).await?;
    crate::db::get_package(db, repo_id, package_name)
        .await?
        .ok_or_else(|| AppError::Internal(format!("failed to create package: {package_name}")))
}

/// Raw markdown, sanitized at render time; capped on a UTF-8 boundary.
async fn store_readme(db: &SqlitePool, package_id: i64, readme: Option<&str>) -> AppResult<()> {
    let Some(readme) = readme.filter(|r| !r.is_empty()) else {
        return Ok(());
    };
    let mut end = readme.len().min(MAX_README_BYTES);
    while !readme.is_char_boundary(end) {
        end -= 1;
    }
    crate::db::update_package_readme(db, package_id, &readme[..end]).await?;
    Ok(())
}

/// Merge the incoming `deprecated` field into an existing version's metadata;
/// npm sends `deprecated: ""` to undeprecate.
async fn apply_metadata_update(
    db: &SqlitePool,
    existing: &Version,
    new_meta: &Value,
) -> AppResult<()> {
    let mut meta: Value =
        serde_json::from_str(&existing.metadata_json).unwrap_or_else(|_| json!({}));
    if let Some(obj) = meta.as_object_mut() {
        match new_meta.get("deprecated") {
            Some(dep) if !matches!(dep, Value::String(s) if s.is_empty()) => {
                obj.insert("deprecated".to_string(), dep.clone());
            }
            _ => {
                obj.remove("deprecated");
            }
        }
    }
    let updated = serde_json::to_string(&meta)?;
    crate::db::update_version_metadata(db, existing.id, &updated).await?;
    Ok(())
}

/// "@acme/httpclient" + "1.0.0" -> "httpclient-1.0.0.tgz"
fn build_tarball_filename(package_name: &str, version: &str) -> String {
    let short_name = package_name
        .split_once('/')
        .map_or(package_name, |(_scope, name)| name);
    format!("{short_name}-{version}.tgz")
}

/// npm keys attachments as "httpclient-1.0.0.tgz" or "@acme/httpclient-1.0.0.tgz".
fn find_attachment_key(
    attachments: &HashMap<String, Attachment>,
    package_name: &str,
    version: &str,
) -> Option<String> {
    let expected = build_tarball_filename(package_name, version);
    if attachments.contains_key(&expected) {
        return Some(expected);
    }
    let scoped = format!("{package_name}-{version}.tgz");
    if attachments.contains_key(&scoped) {
        return Some(scoped);
    }
    attachments.keys().find(|k| k.contains(version)).cloned()
}

fn hex(bytes: impl AsRef<[u8]>) -> String {
    bytes.as_ref().iter().map(|b| format!("{b:02x}")).collect()
}
