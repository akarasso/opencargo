//! `/maven/{repo}/{*path}`: GET and HEAD read, PUT deposits.

use axum::{
    body::Body,
    extract::{Path, State},
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
};
use bytes::Bytes;
use futures_util::StreamExt;

use super::hosted::{self, scope_group, scopes_of, Rendered};
use super::metadata;
use super::path::{ArtifactFile, MavenPath, MetadataLevel, Target};
use crate::app::maven::deposit::{self, digests_of, Deposited};
use crate::auth::middleware::AuthUser;
use crate::domain::{Format, RepoKind, Repository};
use crate::error::{AppError, AppResult};
use crate::ports::maven::{ClientMetadata, SumAlgorithm, UnitKey};
use crate::proxy::Payload;
use crate::server::AppState;

const MAX_SUM_BYTES: usize = 4 * 1024;
const MAX_METADATA_BYTES: usize = 1024 * 1024;

async fn open(state: &AppState, name: &str) -> AppResult<Repository> {
    let repo = crate::registry::load_repo(state.repos.as_ref(), name).await?;
    crate::registry::ensure_format(&repo, Format::Maven)?;
    Ok(repo)
}

fn not_found(path: &str) -> AppError {
    AppError::NotFound(format!("not found: {path}"))
}

fn content_type(name: &str) -> &'static str {
    match name.rsplit('.').next() {
        Some("pom" | "xml") => "application/xml",
        Some("jar" | "war" | "ear" | "aar") => "application/java-archive",
        Some("asc") => "application/pgp-signature",
        Some("json" | "module") => "application/json",
        Some("zip") => "application/zip",
        _ => "application/octet-stream",
    }
}

fn sum_response(value: &str) -> Response {
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
        value.to_string(),
    )
        .into_response()
}

fn matches(headers: &HeaderMap, etag: &str) -> bool {
    headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.split(',').any(|t| t.trim() == etag || t.trim() == "*"))
}

pub(super) fn document(doc: &Rendered, sum: Option<SumAlgorithm>, headers: &HeaderMap) -> Response {
    if let Some(algorithm) = sum {
        return sum_response(digests_of(&doc.body).get(algorithm));
    }
    let mut response = if matches(headers, &doc.etag) {
        StatusCode::NOT_MODIFIED.into_response()
    } else {
        (
            StatusCode::OK,
            [(header::CONTENT_TYPE, "application/xml")],
            doc.body.clone(),
        )
            .into_response()
    };
    if let Ok(etag) = HeaderValue::from_str(&doc.etag) {
        response.headers_mut().insert(header::ETAG, etag);
    }
    if let Some(at) = doc.last_modified {
        let http_date = at.format("%a, %d %b %Y %H:%M:%S GMT").to_string();
        if let Ok(value) = HeaderValue::from_str(&http_date) {
            response.headers_mut().insert(header::LAST_MODIFIED, value);
        }
    }
    response
}

/// GET and HEAD.
pub async fn read(
    State(state): State<AppState>,
    Path((repo_name, path)): Path<(String, String)>,
    auth: Option<axum::Extension<AuthUser>>,
    headers: HeaderMap,
) -> AppResult<Response> {
    let auth = auth.as_ref().map(|e| &e.0);
    let parsed = MavenPath::parse(&path)?;
    let repo = open(&state, &repo_name).await?;
    crate::registry::ensure_can_read(state.permissions.as_ref(), &repo, auth).await?;
    if repo.kind()? != RepoKind::Hosted {
        return Err(not_found(&path));
    }
    match &parsed.target {
        Target::Metadata(dir) => {
            let doc = hosted::metadata(state.maven.as_ref(), state.packages.as_ref(), repo.id, dir)
                .await?
                .ok_or_else(|| not_found(&path))?;
            Ok(document(&doc, parsed.sum, &headers))
        }
        Target::File(file) => {
            let stored = hosted::visible_file(
                state.maven.as_ref(),
                repo.id,
                &file.gav,
                &file.build,
                &file.filename,
            )
            .await?
            .ok_or_else(|| not_found(&path))?;
            if let Some(algorithm) = parsed.sum {
                return Ok(sum_response(stored.digests.get(algorithm)));
            }
            let mut payload = Payload::file(stored.physical_key, stored.size.max(0) as u64);
            payload.content_type = Some(content_type(&file.filename).to_string());
            state.proxy.stream_response(&payload, Vec::new()).await
        }
    }
}

/// PUT: a file, a checksum, or a `maven-metadata.xml`.
pub async fn deposit(
    State(state): State<AppState>,
    Path((repo_name, path)): Path<(String, String)>,
    request: axum::http::Request<Body>,
) -> AppResult<Response> {
    let auth = request
        .extensions()
        .get::<AuthUser>()
        .cloned()
        .ok_or_else(|| AppError::Unauthorized("authentication required".to_string()))?;
    let parsed = MavenPath::parse(&path)?;
    let repo = open(&state, &repo_name).await?;
    crate::registry::ensure_can_write(state.permissions.as_ref(), &repo, &auth).await?;
    crate::registry::ensure_hosted(&repo)?;
    let deposits = state.maven_deposits();
    let now = state.clock.now();
    let body = request.into_body();
    let outcome = match (&parsed.target, parsed.sum) {
        (Target::File(file), None) => {
            let stream = body
                .into_data_stream()
                .map(|chunk| chunk.map_err(|e| e.to_string()));
            let ga = file.gav.ga();
            let scopes = scopes_of(&file.gav);
            let package_path = file.gav.artifact_dir();
            let target = target(repo.id, file, &ga, &package_path, &auth.username, &scopes);
            deposits.file(target, Box::pin(stream), now).await?
        }
        (Target::File(file), Some(algorithm)) => {
            let value = sum_of(body, algorithm).await?;
            let ga = file.gav.ga();
            let scopes = scopes_of(&file.gav);
            let package_path = file.gav.artifact_dir();
            let target = target(repo.id, file, &ga, &package_path, &auth.username, &scopes);
            deposits.sum(target, algorithm, &value, now).await?
        }
        (Target::Metadata(dir), None) => {
            let bytes = axum::body::to_bytes(body, MAX_METADATA_BYTES)
                .await
                .map_err(|e| AppError::BadRequest(format!("failed to read body: {e}")))?;
            let (doc, scopes) = client_document(repo.id, dir, &bytes)?;
            deposits.client_metadata(&doc, &scopes, now).await?
        }
        (Target::Metadata(dir), Some(algorithm)) => {
            let value = sum_of(body, algorithm).await?;
            deposits
                .client_metadata_sum(repo.id, &dir.join("/"), algorithm, &value)
                .await?
        }
    };
    Ok(match outcome {
        Deposited::Stored { .. } => StatusCode::CREATED.into_response(),
        Deposited::Unchanged => StatusCode::OK.into_response(),
    })
}

fn target<'a>(
    repository: i64,
    file: &'a ArtifactFile,
    ga: &'a str,
    package_path: &'a str,
    principal: &'a str,
    scopes: &'a [String],
) -> deposit::Target<'a> {
    deposit::Target {
        unit: UnitKey {
            repository,
            ga,
            version: &file.gav.version,
            build: &file.build,
        },
        package_path,
        filename: &file.filename,
        principal,
        scopes,
    }
}

/// The hex digest a checksum file carries, first token only (some clients
/// append the file name); refused before any state is touched.
async fn sum_of(body: Body, algorithm: SumAlgorithm) -> AppResult<String> {
    let bytes: Bytes = axum::body::to_bytes(body, MAX_SUM_BYTES)
        .await
        .map_err(|e| AppError::BadRequest(format!("failed to read body: {e}")))?;
    let text = std::str::from_utf8(&bytes)
        .map_err(|_| AppError::BadRequest("checksum is not text".to_string()))?;
    let value = text.split_whitespace().next().unwrap_or_default().to_ascii_lowercase();
    if value.len() != algorithm.hex_len() || !value.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(AppError::BadRequest(format!(
            "invalid {} checksum",
            algorithm.as_str()
        )));
    }
    Ok(value)
}

/// The client's document, and the counters it moves: its snapshot's, its
/// artifact's, or its group's for a plugin list.
fn client_document(
    repository: i64,
    dir: &[String],
    body: &[u8],
) -> AppResult<(ClientMetadata, Vec<String>)> {
    let parsed = metadata::parse(body).map_err(|e| AppError::BadRequest(format!("invalid maven-metadata.xml: {e}")))?;
    let scopes = if !parsed.plugins.is_empty() {
        vec![scope_group(&dir.join("."))]
    } else {
        match MetadataLevel::of(dir) {
            Some(MetadataLevel::Snapshot(gav)) => scopes_of(&gav),
            Some(MetadataLevel::Artifact { group, artifact }) => {
                vec![hosted::scope_artifact(&format!("{group}:{artifact}"))]
            }
            None => vec![scope_group(&dir.join("."))],
        }
    };
    let doc = ClientMetadata {
        repository,
        dir: dir.join("/"),
        digests: digests_of(body),
        release: parsed.artifact.release,
        latest: parsed.artifact.latest,
        plugins: parsed.plugins,
    };
    Ok((doc, scopes))
}
