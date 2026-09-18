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
use tracing::info;

use crate::app::publish::Artifact;
use crate::auth::middleware::AuthUser;
use crate::domain::{Format, Package, Repository, Version};
use crate::error::{AppError, AppResult};
use crate::ports::packages::NameMatch;
use crate::registry::extract_package_name;
use crate::app::publish_tail::{PreScan, Published};
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
    filename: String,
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
    crate::registry::rules::rules_of(crate::domain::Format::Npm)?.validate(&package_name)?;
    if body.name != package_name {
        return Err(AppError::BadRequest(format!(
            "package name in body ('{}') does not match URL ('{}')",
            body.name, package_name
        )));
    }

    let repo = crate::registry::load_repo(state.repos.as_ref(), repo_name).await?;
    crate::registry::ensure_hosted(&repo)?;
    crate::registry::ensure_format(&repo, Format::Npm)?;
    crate::registry::ensure_can_write(&*state.permissions, &repo, &auth_user).await?;

    let steps = plan_versions(&state, &repo, &package_name, &body).await?;
    let readme = capped_readme(body.readme.as_deref());

    // A metadata-only publish carries a README too, and its package is
    // already there; one that creates the package carries it in the release.
    if let (Some(readme), Some(package)) = (readme, existing_package(&state, &repo, &package_name).await?)
    {
        state
            .packages
            .set_readme(package.id, readme, chrono::Utc::now())
            .await?;
    }

    for step in steps {
        match step {
            Step::Update { existing, meta } => {
                apply_metadata_update(&state, &existing, &meta).await?
            }
            Step::New(version) => {
                store_version(&state, &repo, &package_name, &body, readme, version, &auth_user)
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
    let existing = state
        .packages
        .package(repo.id, package_name, NameMatch::Exact)
        .await?;
    let mut steps = Vec::with_capacity(body.versions.len());
    for (version_str, version_meta) in &body.versions {
        crate::registry::rules::rules_of(crate::domain::Format::Npm)?.validate_version(version_str)?;

        // A publish carries a tarball attachment; `npm deprecate` does not.
        let attachment = find_attachment_key(&body.attachments, package_name, version_str)
            .and_then(|k| body.attachments.get(&k));

        let existing_version = match &existing {
            Some(package) => state.packages.version(package.id, version_str).await?,
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
    let tarball_url = format!(
        "{}/{repo_name}/{package_name}/-/{tarball_filename}",
        state.base_url
    );
    let meta = with_dist(version_meta, tarball_url, &sha1, &integrity);
    let metadata_json = serde_json::to_string(&meta)?;
    let pre = state.publish_gate().run(Format::Npm, &metadata_json).await?;

    Ok(NewVersion {
        version: version_str.to_string(),
        tarball,
        sha1,
        sha256,
        integrity,
        filename: tarball_filename,
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

#[allow(clippy::too_many_arguments)]
async fn store_version(
    state: &AppState,
    repo: &Repository,
    package_name: &str,
    body: &PublishBody,
    readme: Option<&str>,
    v: NewVersion,
    auth_user: &AuthUser,
) -> AppResult<()> {
    let size = v.tarball.len() as i64;
    let tags = tags_for(&body.dist_tags, &v.version);
    let landed = state.publish_version()
        .run(
            Artifact {
                repository: repo.id,
                package: package_name,
                match_name: NameMatch::Exact,
                description: body.description.as_deref(),
                readme,
                version: &v.version,
                metadata_json: &v.metadata_json,
                checksum_sha1: Some(&v.sha1),
                checksum_sha256: Some(&v.sha256),
                integrity: Some(&v.integrity),
                filename: &v.filename,
                dist_tags: &tags,
                bytes: v.tarball,
            },
            chrono::Utc::now(),
        )
        .await?;

    let (package, version_id) = (landed.package, landed.version.id);
    record_dependencies(state.deps.as_ref(), package.id, version_id, &v.meta).await;

    state
        .publish_tail()
        .run(
            &Published {
                format: Format::Npm,
                repository: &repo.name,
                package: &package.name,
                version: &v.version,
                version_id: Some(version_id),
                metadata_json: &v.metadata_json,
                published_by: &auth_user.username,
            },
            v.pre,
            chrono::Utc::now(),
        )
        .await;

    info!(
        package = %package.name,
        version = %v.version,
        size,
        repo = %repo.name,
        "Package version published"
    );
    Ok(())
}

/// The tags the body points at this version.
fn tags_for(dist_tags: &HashMap<String, String>, version: &str) -> Vec<String> {
    dist_tags
        .iter()
        .filter(|(_, at)| at.as_str() == version)
        .map(|(tag, _)| tag.clone())
        .collect()
}

async fn record_dependencies(
    deps: &dyn crate::ports::deps::DependencyStore,
    package_id: i64,
    version_id: i64,
    meta: &Value,
) {
    const DEP_TYPES: [(&str, &str); 4] = [
        ("dependencies", "runtime"),
        ("devDependencies", "dev"),
        ("peerDependencies", "peer"),
        ("optionalDependencies", "optional"),
    ];
    for (field, dep_type) in DEP_TYPES {
        let Some(entries) = meta.get(field).and_then(|v| v.as_object()) else {
            continue;
        };
        for (dep_name, dep_version) in entries {
            let dep = crate::ports::deps::NewDependency {
                package: package_id,
                version: version_id,
                name: dep_name,
                requirement: dep_version.as_str().unwrap_or("*"),
                kind: dep_type,
            };
            if let Err(e) = deps.record(&dep, chrono::Utc::now()).await {
                tracing::warn!(
                    dependency = %dep_name,
                    "failed to record dependency (graph may be incomplete): {e}"
                );
            }
        }
    }
}

async fn existing_package(
    state: &AppState,
    repo: &Repository,
    package_name: &str,
) -> AppResult<Option<Package>> {
    Ok(state
        .packages
        .package(repo.id, package_name, NameMatch::Exact)
        .await?)
}

/// Raw markdown, sanitized at render time; capped on a UTF-8 boundary.
fn capped_readme(readme: Option<&str>) -> Option<&str> {
    let readme = readme.filter(|r| !r.is_empty())?;
    let mut end = readme.len().min(MAX_README_BYTES);
    while !readme.is_char_boundary(end) {
        end -= 1;
    }
    Some(&readme[..end])
}

/// Merge the incoming `deprecated` field into an existing version's metadata;
/// npm sends `deprecated: ""` to undeprecate.
async fn apply_metadata_update(
    state: &AppState,
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
    state.packages.set_metadata(existing.id, &updated).await?;
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
