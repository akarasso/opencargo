use std::collections::HashMap;

use axum::{
    body::Bytes,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    Json,
};
use base64::Engine;
use serde::Deserialize;
use serde_json::{json, Value};
use sha1::Digest;
use tracing::{info, warn};

use crate::auth::middleware::AuthUser;
use crate::auth::permissions::check_repo_permission;
use crate::error::{AppError, AppResult};
use crate::proxy;
use crate::server::AppState;
use crate::storage::StorageBackend;

// ---------------------------------------------------------------------------
// Publish — PUT /{repo}/@{scope}/{name} or PUT /{repo}/{name}
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct PublishBody {
    #[allow(dead_code)]
    name: String,
    description: Option<String>,
    #[serde(rename = "dist-tags", default)]
    dist_tags: HashMap<String, String>,
    #[serde(default)]
    versions: HashMap<String, Value>,
    #[serde(rename = "_attachments", default)]
    attachments: HashMap<String, Attachment>,
}

#[derive(Deserialize)]
pub struct Attachment {
    data: String,
    #[allow(dead_code)]
    length: Option<u64>,
}

pub async fn publish_package(
    State(state): State<AppState>,
    Path(params): Path<HashMap<String, String>>,
    request: axum::http::Request<axum::body::Body>,
) -> AppResult<impl IntoResponse> {
    // Require authentication
    let auth_user = request
        .extensions()
        .get::<AuthUser>()
        .cloned()
        .ok_or_else(|| AppError::Unauthorized("authentication required".to_string()))?;

    // Rate limit: 30 publishes per minute per user
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

    let repo_name = params.get("repo").ok_or_else(|| {
        AppError::BadRequest("missing repository".to_string())
    })?;
    let package_name = extract_package_name(&params);

    // Validate repo exists and is hosted
    let repo = crate::db::get_repository_by_name(&state.db, repo_name)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("repository not found: {repo_name}")))?;

    if repo.repo_type != "hosted" {
        return Err(AppError::BadRequest(
            "can only publish to hosted repositories".to_string(),
        ));
    }

    // Check granular write permission on this repository
    if !check_repo_permission(&state.db, auth_user.user_id, &auth_user.role, repo.id, "write").await {
        return Err(AppError::Forbidden("insufficient permissions".to_string()));
    }

    // Get or create the package
    let package = match crate::db::get_package(&state.db, repo.id, &package_name).await? {
        Some(p) => p,
        None => {
            let _id = crate::db::create_package(
                &state.db,
                repo.id,
                &package_name,
                body.description.as_deref(),
            )
            .await?;
            crate::db::get_package(&state.db, repo.id, &package_name)
                .await?
                .ok_or_else(|| {
                    AppError::Internal(format!("failed to create package: {package_name}"))
                })?
        }
    };

    // Process each version in the publish body
    for (version_str, version_meta) in &body.versions {
        // Check if version already exists
        if let Some(_) =
            crate::db::get_version(&state.db, package.id, version_str).await?
        {
            return Err(AppError::Conflict(format!(
                "version {version_str} already exists for {package_name}"
            )));
        }

        // Find the attachment for this version
        let attachment_key = find_attachment_key(&body.attachments, &package_name, version_str);
        let attachment = attachment_key
            .and_then(|k| body.attachments.get(&k))
            .ok_or_else(|| {
                AppError::BadRequest(format!(
                    "no attachment found for version {version_str}"
                ))
            })?;

        // Decode the base64 tarball
        let tarball_data = base64::engine::general_purpose::STANDARD
            .decode(&attachment.data)
            .map_err(|e| AppError::BadRequest(format!("invalid base64 attachment: {e}")))?;

        // Compute checksums
        let sha1_hex = hex::encode(sha1::Sha1::digest(&tarball_data));
        let sha256_hex = hex::encode(sha2::Sha256::digest(&tarball_data));
        let integrity = format!(
            "sha512-{}",
            base64::engine::general_purpose::STANDARD
                .encode(sha2::Sha512::digest(&tarball_data))
        );

        // Verify shasum if provided
        if let Some(dist) = version_meta.get("dist") {
            if let Some(expected_shasum) = dist.get("shasum").and_then(|v| v.as_str()) {
                if !expected_shasum.is_empty() && expected_shasum != sha1_hex {
                    return Err(AppError::BadRequest(format!(
                        "shasum mismatch: expected {expected_shasum}, got {sha1_hex}"
                    )));
                }
            }
        }

        // Build the tarball filename and storage path
        let tarball_filename = build_tarball_filename(&package_name, version_str);
        let storage_path = format!(
            "npm/{}/{}/{}",
            repo_name,
            package_name.replace('/', "/"),
            tarball_filename
        );

        // Store the tarball
        state
            .storage
            .put(&storage_path, Bytes::from(tarball_data.clone()))
            .await?;

        // Rewrite dist in version metadata to point to our server
        let mut meta = version_meta.clone();
        let tarball_url = format!(
            "{}/{}/{}/-/{}",
            state.base_url, repo_name, package_name, tarball_filename
        );
        if let Some(dist) = meta.get_mut("dist") {
            if let Some(obj) = dist.as_object_mut() {
                obj.insert("tarball".to_string(), Value::String(tarball_url));
                obj.insert("shasum".to_string(), Value::String(sha1_hex.clone()));
                obj.insert(
                    "integrity".to_string(),
                    Value::String(integrity.clone()),
                );
            }
        } else {
            meta.as_object_mut().map(|obj| {
                obj.insert(
                    "dist".to_string(),
                    json!({
                        "tarball": tarball_url,
                        "shasum": sha1_hex.clone(),
                        "integrity": integrity.clone(),
                    }),
                );
            });
        }

        let metadata_json = serde_json::to_string(&meta)?;
        let size = tarball_data.len() as i64;

        // Insert version in DB
        let version_id = crate::db::create_version(
            &state.db,
            package.id,
            version_str,
            &metadata_json,
            Some(&sha1_hex),
            Some(&sha256_hex),
            Some(&integrity),
            size,
            &storage_path,
        )
        .await?;

        // Set dist-tags
        for (tag, tag_version) in &body.dist_tags {
            if tag_version == version_str {
                crate::db::set_dist_tag(&state.db, package.id, tag, version_id)
                    .await?;
            }
        }

        // Extract dependencies and store in package_dependencies table
        let dep_types = [
            ("dependencies", "runtime"),
            ("devDependencies", "dev"),
            ("peerDependencies", "peer"),
            ("optionalDependencies", "optional"),
        ];
        for (field, dep_type) in &dep_types {
            if let Some(deps_obj) = version_meta.get(*field).and_then(|v| v.as_object()) {
                for (dep_name, dep_version) in deps_obj {
                    let version_req = dep_version.as_str().unwrap_or("*");
                    let _ = crate::db::insert_dependency(
                        &state.db,
                        package.id,
                        version_id,
                        dep_name,
                        version_req,
                        dep_type,
                    )
                    .await;
                }
            }
        }

        // Dispatch webhook for package.published
        state.webhook_dispatcher.dispatch("package.published", &json!({
            "package": package_name,
            "version": version_str,
            "repository": repo_name,
            "published_by": auth_user.username,
        })).await;

        // Vulnerability scan
        if state.vuln_scan_config.block_on_critical {
            // Blocking scan: check for critical vulns before accepting
            let scan_result = state
                .vuln_scanner
                .scan_version(&state.db, version_id, &metadata_json, "npm")
                .await;
            if let Ok(ref result) = scan_result {
                if result.status == "critical" {
                    return Err(AppError::BadRequest(
                        "publish blocked: critical vulnerabilities found in dependencies".to_string(),
                    ));
                }
            }
        } else {
            // Non-blocking background scan
            let scanner = state.vuln_scanner.clone();
            let db = state.db.clone();
            let meta_json = metadata_json.clone();
            tokio::spawn(async move {
                if let Err(e) = scanner.scan_version(&db, version_id, &meta_json, "npm").await {
                    warn!(error = %e, "Background vulnerability scan failed");
                }
            });
        }

        info!(
            package = %package_name,
            version = %version_str,
            size = size,
            repo = %repo_name,
            "Package version published"
        );
    }

    Ok((StatusCode::OK, Json(json!({"ok": true}))))
}

// ---------------------------------------------------------------------------
// Get metadata — GET /{repo}/@{scope}/{name}
// ---------------------------------------------------------------------------

pub async fn get_package(
    State(state): State<AppState>,
    Path(params): Path<HashMap<String, String>>,
    headers: HeaderMap,
) -> AppResult<impl IntoResponse> {
    let repo_name = params.get("repo").ok_or_else(|| {
        AppError::BadRequest("missing repository".to_string())
    })?;
    let package_name = extract_package_name(&params);

    let repo = crate::db::get_repository_by_name(&state.db, repo_name)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("repository not found: {repo_name}")))?;

    let abbreviated = headers
        .get("accept")
        .and_then(|v| v.to_str().ok())
        .map(|v| v.contains("application/vnd.npm.install-v1+json"))
        .unwrap_or(false);

    match repo.repo_type.as_str() {
        "proxy" => {
            get_package_proxy(&state, &repo, &package_name, repo_name, abbreviated).await
        }
        "group" => {
            get_package_group(&state, &repo, &package_name, repo_name, abbreviated, 0).await
        }
        _ => {
            // "hosted" — original logic
            get_package_hosted(&state, &repo, &package_name, abbreviated).await
        }
    }
}

/// Serve package metadata from a hosted repository (the original Phase 1 logic).
async fn get_package_hosted(
    state: &AppState,
    repo: &crate::db::Repository,
    package_name: &str,
    abbreviated: bool,
) -> AppResult<axum::response::Response> {
    let package = crate::db::get_package(&state.db, repo.id, package_name)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("package not found: {package_name}")))?;

    let versions = crate::db::get_versions(&state.db, package.id).await?;
    let dist_tags = crate::db::get_dist_tags(&state.db, package.id).await?;

    // Build dist-tags map
    let mut dist_tags_map: HashMap<String, String> = HashMap::new();
    for dt in &dist_tags {
        if let Some(v) = versions.iter().find(|v| v.id == dt.version_id) {
            dist_tags_map.insert(dt.tag.clone(), v.version.clone());
        }
    }

    // Build versions map
    let mut versions_map: HashMap<String, Value> = HashMap::new();
    let mut time_map: HashMap<String, String> = HashMap::new();

    time_map.insert("created".to_string(), package.created_at.clone());
    time_map.insert("modified".to_string(), package.updated_at.clone());

    for v in &versions {
        let mut meta: Value = serde_json::from_str(&v.metadata_json).unwrap_or(json!({}));

        if abbreviated {
            strip_to_abbreviated(&mut meta);
        }

        time_map.insert(v.version.clone(), v.published_at.clone());
        versions_map.insert(v.version.clone(), meta);
    }

    let response = json!({
        "_id": package_name,
        "name": package_name,
        "description": package.description,
        "dist-tags": dist_tags_map,
        "versions": versions_map,
        "time": time_map,
    });

    if abbreviated {
        Ok((
            [(
                "content-type",
                "application/vnd.npm.install-v1+json",
            )],
            Json(response),
        )
            .into_response())
    } else {
        Ok(Json(response).into_response())
    }
}

/// Serve package metadata from a proxy repository.
/// Fetches from upstream when the cache is missing or stale,
/// rewrites tarball URLs to point to our server.
async fn get_package_proxy(
    state: &AppState,
    repo: &crate::db::Repository,
    package_name: &str,
    repo_name: &str,
    abbreviated: bool,
) -> AppResult<axum::response::Response> {
    let upstream_url = repo.upstream_url.as_deref().ok_or_else(|| {
        AppError::Internal(format!(
            "proxy repository {repo_name} has no upstream_url configured"
        ))
    })?;

    let ttl_seconds: u64 = 86400; // default 24h

    let mut metadata = state
        .proxy_client
        .fetch_package_metadata(repo_name, upstream_url, package_name, repo.id, ttl_seconds)
        .await?;

    // Rewrite tarball URLs to point to our server
    proxy::rewrite_tarball_urls(&mut metadata, &state.base_url, repo_name, package_name);

    if abbreviated {
        // Strip version fields for abbreviated response
        if let Some(versions) = metadata.get_mut("versions").and_then(|v| v.as_object_mut()) {
            for (_key, version_meta) in versions.iter_mut() {
                strip_to_abbreviated(version_meta);
            }
        }
        Ok((
            [(
                "content-type",
                "application/vnd.npm.install-v1+json",
            )],
            Json(metadata),
        )
            .into_response())
    } else {
        Ok(Json(metadata).into_response())
    }
}

/// Serve package metadata from a group repository.
/// Tries each member repo in order and returns the first hit.
fn get_package_group<'a>(
    state: &'a AppState,
    repo: &'a crate::db::Repository,
    package_name: &'a str,
    repo_name: &'a str,
    abbreviated: bool,
    depth: u32,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = AppResult<axum::response::Response>> + Send + 'a>>
{
    Box::pin(async move {
        if depth > 5 {
            return Err(AppError::Internal(
                "group nesting depth exceeded (max 5)".to_string(),
            ));
        }

        let members = crate::db::parse_group_members(repo.config_json.as_deref());

        if members.is_empty() {
            return Err(AppError::Internal(format!(
                "group repository {repo_name} has no members configured"
            )));
        }

        for member_name in &members {
            let member_repo =
                match crate::db::get_repository_by_name(&state.db, member_name).await? {
                    Some(r) => r,
                    None => {
                        warn!(
                            group = %repo_name,
                            member = %member_name,
                            "Group member repository not found, skipping"
                        );
                        continue;
                    }
                };

            let result = match member_repo.repo_type.as_str() {
                "proxy" => {
                    get_package_proxy(state, &member_repo, package_name, member_name, abbreviated)
                        .await
                }
                "hosted" => {
                    get_package_hosted(state, &member_repo, package_name, abbreviated).await
                }
                "group" => {
                    // Nested groups — recurse (boxed)
                    get_package_group(state, &member_repo, package_name, member_name, abbreviated, depth + 1)
                        .await
                }
                _ => continue,
            };

            match result {
                Ok(response) => return Ok(response),
                Err(AppError::NotFound(_)) => continue, // try next member
                Err(e) => {
                    warn!(
                        group = %repo_name,
                        member = %member_name,
                        error = %e,
                        "Error fetching from group member, trying next"
                    );
                    continue;
                }
            }
        }

        Err(AppError::NotFound(format!(
            "package not found in any member of group {repo_name}: {package_name}"
        )))
    })
}

/// Strip version metadata fields to only those needed for abbreviated install responses.
fn strip_to_abbreviated(meta: &mut Value) {
    if let Some(obj) = meta.as_object_mut() {
        let keep_fields = [
            "name",
            "version",
            "dependencies",
            "devDependencies",
            "peerDependencies",
            "optionalDependencies",
            "bin",
            "directories",
            "engines",
            "dist",
            "bundleDependencies",
            "peerDependenciesMeta",
        ];
        let keys: Vec<String> = obj.keys().cloned().collect();
        for key in keys {
            if !keep_fields.contains(&key.as_str()) {
                obj.remove(&key);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Download tarball — GET /{repo}/@{scope}/{name}/-/{filename}
// ---------------------------------------------------------------------------

pub async fn download_tarball(
    State(state): State<AppState>,
    Path(params): Path<HashMap<String, String>>,
) -> AppResult<impl IntoResponse> {
    let repo_name = params.get("repo").ok_or_else(|| {
        AppError::BadRequest("missing repository".to_string())
    })?;
    let package_name = extract_package_name(&params);
    let filename = params.get("filename").ok_or_else(|| {
        AppError::BadRequest("missing filename".to_string())
    })?;

    let repo = crate::db::get_repository_by_name(&state.db, repo_name)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("repository not found: {repo_name}")))?;

    match repo.repo_type.as_str() {
        "proxy" => {
            download_tarball_proxy(&state, &repo, &package_name, filename, repo_name).await
        }
        "group" => {
            download_tarball_group(&state, &repo, &package_name, filename, repo_name, 0).await
        }
        _ => {
            // "hosted"
            download_tarball_hosted(&state, &repo, &package_name, filename).await
        }
    }
}

/// Download tarball from a hosted repository.
async fn download_tarball_hosted(
    state: &AppState,
    repo: &crate::db::Repository,
    package_name: &str,
    filename: &str,
) -> AppResult<axum::response::Response> {
    let package = crate::db::get_package(&state.db, repo.id, package_name)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("package not found: {package_name}")))?;

    // Find the version by matching the tarball filename
    let versions = crate::db::get_versions(&state.db, package.id).await?;
    let version = versions
        .iter()
        .find(|v| v.tarball_path.ends_with(filename))
        .ok_or_else(|| AppError::NotFound(format!("tarball not found: {filename}")))?;

    // Record download
    let _ = crate::db::record_download(&state.db, version.id).await;

    // Read from storage
    let data = state.storage.get(&version.tarball_path).await?;
    let safe_filename = filename.replace('"', "").replace('\n', "").replace('\r', "");
    let disposition = format!("attachment; filename=\"{safe_filename}\"");

    Ok((
        StatusCode::OK,
        [
            (
                axum::http::header::CONTENT_TYPE,
                "application/octet-stream".to_string(),
            ),
            (
                axum::http::header::CONTENT_DISPOSITION,
                disposition,
            ),
        ],
        data,
    )
        .into_response())
}

/// Download tarball from a proxy repository.
/// If the tarball is not cached locally, fetch from upstream, cache, and serve.
async fn download_tarball_proxy(
    state: &AppState,
    repo: &crate::db::Repository,
    package_name: &str,
    filename: &str,
    repo_name: &str,
) -> AppResult<axum::response::Response> {
    let upstream_url = repo.upstream_url.as_deref().ok_or_else(|| {
        AppError::Internal(format!(
            "proxy repository {repo_name} has no upstream_url configured"
        ))
    })?;

    let data = state
        .proxy_client
        .fetch_tarball(repo_name, upstream_url, package_name, filename, repo.id)
        .await?;

    let safe_filename = filename.replace('"', "").replace('\n', "").replace('\r', "");
    let disposition = format!("attachment; filename=\"{safe_filename}\"");

    Ok((
        StatusCode::OK,
        [
            (
                axum::http::header::CONTENT_TYPE,
                "application/octet-stream".to_string(),
            ),
            (
                axum::http::header::CONTENT_DISPOSITION,
                disposition,
            ),
        ],
        data,
    )
        .into_response())
}

/// Download tarball from a group repository.
/// Route to the correct member repo that has the tarball.
fn download_tarball_group<'a>(
    state: &'a AppState,
    repo: &'a crate::db::Repository,
    package_name: &'a str,
    filename: &'a str,
    repo_name: &'a str,
    depth: u32,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = AppResult<axum::response::Response>> + Send + 'a>>
{
    Box::pin(async move {
        if depth > 5 {
            return Err(AppError::Internal(
                "group nesting depth exceeded (max 5)".to_string(),
            ));
        }

        let members = crate::db::parse_group_members(repo.config_json.as_deref());

        if members.is_empty() {
            return Err(AppError::Internal(format!(
                "group repository {repo_name} has no members configured"
            )));
        }

        for member_name in &members {
            let member_repo =
                match crate::db::get_repository_by_name(&state.db, member_name).await? {
                    Some(r) => r,
                    None => continue,
                };

            let result = match member_repo.repo_type.as_str() {
                "proxy" => {
                    download_tarball_proxy(
                        state,
                        &member_repo,
                        package_name,
                        filename,
                        member_name,
                    )
                    .await
                }
                "hosted" => {
                    download_tarball_hosted(state, &member_repo, package_name, filename).await
                }
                "group" => {
                    download_tarball_group(
                        state,
                        &member_repo,
                        package_name,
                        filename,
                        member_name,
                        depth + 1,
                    )
                    .await
                }
                _ => continue,
            };

            match result {
                Ok(response) => return Ok(response),
                Err(AppError::NotFound(_)) => continue,
                Err(e) => {
                    warn!(
                        group = %repo_name,
                        member = %member_name,
                        error = %e,
                        "Error downloading tarball from group member, trying next"
                    );
                    continue;
                }
            }
        }

        Err(AppError::NotFound(format!(
            "tarball not found in any member of group {repo_name}: {filename}"
        )))
    })
}

// ---------------------------------------------------------------------------
// Search — GET /{repo}/-/v1/search
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct SearchQuery {
    text: Option<String>,
    size: Option<i64>,
    from: Option<i64>,
}

pub async fn search(
    State(state): State<AppState>,
    Path(repo_name): Path<String>,
    Query(query): Query<SearchQuery>,
) -> AppResult<impl IntoResponse> {
    let repo = crate::db::get_repository_by_name(&state.db, &repo_name)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("repository not found: {repo_name}")))?;

    let search_text = query.text.unwrap_or_default();
    let size = query.size.unwrap_or(20).min(250);
    let from = query.from.unwrap_or(0);

    match repo.repo_type.as_str() {
        "group" => {
            // Merge search results from all member repos
            let members = crate::db::parse_group_members(repo.config_json.as_deref());
            let mut all_objects = Vec::new();
            let mut seen_names = std::collections::HashSet::new();

            for member_name in &members {
                let member_repo =
                    match crate::db::get_repository_by_name(&state.db, member_name).await? {
                        Some(r) => r,
                        None => continue,
                    };

                let member_objects =
                    search_in_repo(&state, member_repo.id, &search_text, size, from).await?;

                for obj in member_objects {
                    // Deduplicate by package name
                    if let Some(name) = obj
                        .get("package")
                        .and_then(|p| p.get("name"))
                        .and_then(|n| n.as_str())
                    {
                        if seen_names.insert(name.to_string()) {
                            all_objects.push(obj);
                        }
                    } else {
                        all_objects.push(obj);
                    }
                }
            }

            // Apply pagination to merged results
            let total = all_objects.len();
            let from_idx = (from as usize).min(total);
            let end_idx = (from_idx + size as usize).min(total);
            let page = &all_objects[from_idx..end_idx];

            Ok(Json(json!({
                "objects": page,
                "total": total,
                "time": "0ms",
            })))
        }
        _ => {
            // "hosted" or "proxy" — search local DB
            let objects = search_in_repo(&state, repo.id, &search_text, size, from).await?;
            let total = objects.len();

            Ok(Json(json!({
                "objects": objects,
                "total": total,
                "time": "0ms",
            })))
        }
    }
}

/// Search for packages in a single repository by ID.
///
/// Uses FTS5 full-text search when available, falling back to LIKE queries
/// if the FTS query fails (e.g., special characters or FTS5 not available).
async fn search_in_repo(
    state: &AppState,
    repo_id: i64,
    search_text: &str,
    size: i64,
    from: i64,
) -> Result<Vec<Value>, AppError> {
    // Try FTS5 first
    let packages: Vec<crate::db::Package> = if !search_text.is_empty() {
        // Sanitize the search text for FTS5: wrap each word in double quotes to avoid
        // syntax errors from special characters
        let fts_query = search_text
            .split_whitespace()
            .map(|word| format!("\"{}\"", word.replace('"', "")))
            .collect::<Vec<_>>()
            .join(" ");

        let fts_result: Result<Vec<crate::db::Package>, _> = sqlx::query_as(
            "SELECT p.* FROM packages p \
             JOIN packages_fts fts ON p.id = fts.rowid \
             WHERE p.repository_id = ?1 AND packages_fts MATCH ?2 \
             ORDER BY rank \
             LIMIT ?3 OFFSET ?4",
        )
        .bind(repo_id)
        .bind(&fts_query)
        .bind(size)
        .bind(from)
        .fetch_all(&state.db)
        .await;

        match fts_result {
            Ok(pkgs) => pkgs,
            Err(_) => {
                // Fallback to LIKE if FTS query fails
                let pattern = format!("%{search_text}%");
                sqlx::query_as(
                    "SELECT * FROM packages WHERE repository_id = ?1 AND (name LIKE ?2 OR description LIKE ?2) LIMIT ?3 OFFSET ?4",
                )
                .bind(repo_id)
                .bind(&pattern)
                .bind(size)
                .bind(from)
                .fetch_all(&state.db)
                .await?
            }
        }
    } else {
        let pattern = format!("%{search_text}%");
        sqlx::query_as(
            "SELECT * FROM packages WHERE repository_id = ?1 AND (name LIKE ?2 OR description LIKE ?2) LIMIT ?3 OFFSET ?4",
        )
        .bind(repo_id)
        .bind(&pattern)
        .bind(size)
        .bind(from)
        .fetch_all(&state.db)
        .await?
    };

    let mut objects = Vec::new();
    for pkg in &packages {
        let dist_tags = crate::db::get_dist_tags(&state.db, pkg.id).await?;
        let versions = crate::db::get_versions(&state.db, pkg.id).await?;

        let latest_version = dist_tags
            .iter()
            .find(|dt| dt.tag == "latest")
            .and_then(|dt| versions.iter().find(|v| v.id == dt.version_id))
            .or_else(|| versions.last());

        objects.push(json!({
            "package": {
                "name": pkg.name,
                "description": pkg.description,
                "version": latest_version.map(|v| v.version.as_str()).unwrap_or("0.0.0"),
                "date": latest_version.map(|v| v.published_at.as_str()).unwrap_or(""),
            },
        }));
    }

    Ok(objects)
}

// ---------------------------------------------------------------------------
// Dist-tags
// ---------------------------------------------------------------------------

pub async fn get_dist_tags(
    State(state): State<AppState>,
    Path(params): Path<HashMap<String, String>>,
) -> AppResult<impl IntoResponse> {
    let repo_name = params.get("repo").ok_or_else(|| {
        AppError::BadRequest("missing repository".to_string())
    })?;
    let package_name = extract_package_name(&params);

    let repo = crate::db::get_repository_by_name(&state.db, repo_name)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("repository not found: {repo_name}")))?;

    let package = crate::db::get_package(&state.db, repo.id, &package_name)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("package not found: {package_name}")))?;

    let dist_tags = crate::db::get_dist_tags(&state.db, package.id).await?;
    let versions = crate::db::get_versions(&state.db, package.id).await?;

    let mut tags_map: HashMap<String, String> = HashMap::new();
    for dt in &dist_tags {
        if let Some(v) = versions.iter().find(|v| v.id == dt.version_id) {
            tags_map.insert(dt.tag.clone(), v.version.clone());
        }
    }

    Ok(Json(json!(tags_map)))
}

pub async fn put_dist_tag(
    State(state): State<AppState>,
    Path(params): Path<HashMap<String, String>>,
    auth_user: Option<axum::Extension<AuthUser>>,
    body: Bytes,
) -> AppResult<impl IntoResponse> {
    // Require authentication
    let user = auth_user
        .ok_or_else(|| AppError::Unauthorized("authentication required".to_string()))?
        .0;

    let repo_name = params.get("repo").ok_or_else(|| {
        AppError::BadRequest("missing repository".to_string())
    })?;
    let package_name = extract_package_name(&params);
    let tag = params.get("tag").ok_or_else(|| {
        AppError::BadRequest("missing tag".to_string())
    })?;

    // Body is the version string, JSON-encoded (e.g., "\"1.0.0\"")
    let version_str: String = serde_json::from_slice(&body)
        .map_err(|_| AppError::BadRequest("invalid version string".to_string()))?;

    let repo = crate::db::get_repository_by_name(&state.db, repo_name)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("repository not found: {repo_name}")))?;

    // Check granular write permission on this repository
    if !check_repo_permission(&state.db, user.user_id, &user.role, repo.id, "write").await {
        return Err(AppError::Forbidden("insufficient permissions".to_string()));
    }

    let package = crate::db::get_package(&state.db, repo.id, &package_name)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("package not found: {package_name}")))?;

    let version = crate::db::get_version(&state.db, package.id, &version_str)
        .await?
        .ok_or_else(|| {
            AppError::NotFound(format!("version not found: {version_str}"))
        })?;

    crate::db::set_dist_tag(&state.db, package.id, tag, version.id).await?;

    Ok(Json(json!({"ok": true})))
}

pub async fn delete_dist_tag(
    State(state): State<AppState>,
    Path(params): Path<HashMap<String, String>>,
    auth_user: Option<axum::Extension<AuthUser>>,
) -> AppResult<impl IntoResponse> {
    // Require authentication
    let user = auth_user
        .ok_or_else(|| AppError::Unauthorized("authentication required".to_string()))?
        .0;

    let repo_name = params.get("repo").ok_or_else(|| {
        AppError::BadRequest("missing repository".to_string())
    })?;
    let package_name = extract_package_name(&params);
    let tag = params.get("tag").ok_or_else(|| {
        AppError::BadRequest("missing tag".to_string())
    })?;

    let repo = crate::db::get_repository_by_name(&state.db, repo_name)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("repository not found: {repo_name}")))?;

    // Check granular write permission on this repository
    if !check_repo_permission(&state.db, user.user_id, &user.role, repo.id, "write").await {
        return Err(AppError::Forbidden("insufficient permissions".to_string()));
    }

    let package = crate::db::get_package(&state.db, repo.id, &package_name)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("package not found: {package_name}")))?;

    sqlx::query("DELETE FROM dist_tags WHERE package_id = ?1 AND tag = ?2")
        .bind(package.id)
        .bind(tag.as_str())
        .execute(&state.db)
        .await?;

    Ok(Json(json!({"ok": true})))
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Extract the full package name from path parameters.
/// For scoped packages: scope="trace", name="httpclient" → "@trace/httpclient"
/// For unscoped packages: name="react" → "react"
fn extract_package_name(params: &HashMap<String, String>) -> String {
    match params.get("scope") {
        Some(scope) => format!("@{}/{}", scope, params.get("name").unwrap_or(&String::new())),
        None => params
            .get("name")
            .cloned()
            .unwrap_or_default(),
    }
}

/// Build the tarball filename from package name and version.
/// "@trace/httpclient" + "1.0.0" → "httpclient-1.0.0.tgz"
/// "react" + "18.0.0" → "react-18.0.0.tgz"
fn build_tarball_filename(package_name: &str, version: &str) -> String {
    let short_name = if let Some((_scope, name)) = package_name.split_once('/') {
        name
    } else {
        package_name
    };
    format!("{short_name}-{version}.tgz")
}

/// Find the attachment key matching a version.
/// npm uses keys like "httpclient-1.0.0.tgz" or "@trace/httpclient-1.0.0.tgz"
fn find_attachment_key(
    attachments: &HashMap<String, Attachment>,
    package_name: &str,
    version: &str,
) -> Option<String> {
    let expected = build_tarball_filename(package_name, version);
    // Try exact match first
    if attachments.contains_key(&expected) {
        return Some(expected);
    }
    // Try with scope prefix
    let scoped = format!("{package_name}-{version}.tgz");
    if attachments.contains_key(&scoped) {
        return Some(scoped);
    }
    // Fallback: find any attachment that contains the version
    attachments
        .keys()
        .find(|k| k.contains(version))
        .cloned()
}

/// Encode bytes as hex string.
mod hex {
    pub fn encode(bytes: impl AsRef<[u8]>) -> String {
        bytes
            .as_ref()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    }
}
