use axum::{
    extract::{Query, State},
    http::StatusCode,
    response::IntoResponse,
    Json,
};
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;

use crate::server::AppState;

// ---------------------------------------------------------------------------
// Response types
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct DashboardResponse {
    total_packages: i64,
    total_versions: i64,
    total_downloads: i64,
    total_repos: i64,
    recent_versions: Vec<RecentVersionResponse>,
}

#[derive(Serialize)]
struct RecentVersionResponse {
    package_name: String,
    version: String,
    published_at: String,
}

#[derive(Serialize)]
struct RepositoriesResponse {
    repositories: Vec<RepoResponse>,
}

#[derive(Serialize)]
struct RepoResponse {
    name: String,
}

#[derive(Serialize)]
struct PackagesResponse {
    packages: Vec<PackageResponse>,
    total: i64,
    page: i64,
    page_size: i64,
    has_next: bool,
}

#[derive(Serialize)]
struct PackageResponse {
    name: String,
    latest_version: String,
    description: String,
    downloads: i64,
    published_at: String,
}

#[derive(Serialize)]
struct PackageDetailResponse {
    name: String,
    description: String,
    license: String,
    readme_html: String,
    total_downloads: i64,
    versions: Vec<VersionResponse>,
    dist_tags: Vec<DistTagResponse>,
}

#[derive(Serialize)]
struct VersionResponse {
    version: String,
    size_display: String,
    published_at: String,
}

#[derive(Serialize)]
struct DistTagResponse {
    tag: String,
    version: String,
}

#[derive(Serialize)]
struct SearchResponse {
    query: String,
    results: Vec<SearchResultResponse>,
}

#[derive(Serialize)]
struct SearchResultResponse {
    name: String,
    latest_version: String,
    description: String,
}

// ---------------------------------------------------------------------------
// Query params
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct PackagesQuery {
    #[serde(default)]
    q: String,
    #[serde(default)]
    repo: String,
    #[serde(default = "default_page")]
    page: i64,
}

fn default_page() -> i64 {
    1
}

#[derive(Debug, Deserialize)]
pub struct SearchQuery {
    #[serde(default)]
    q: String,
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

const PAGE_SIZE: i64 = 20;

fn format_size(bytes: i64) -> String {
    if bytes < 1024 {
        format!("{} B", bytes)
    } else if bytes < 1024 * 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    }
}

fn sanitize_html(html: &str) -> String {
    html.replace("<script", "&lt;script")
        .replace("<Script", "&lt;Script")
        .replace("<SCRIPT", "&lt;SCRIPT")
        .replace("<iframe", "&lt;iframe")
        .replace("<IFRAME", "&lt;IFRAME")
        .replace("<object", "&lt;object")
        .replace("<OBJECT", "&lt;OBJECT")
        .replace("<embed", "&lt;embed")
        .replace("<EMBED", "&lt;EMBED")
        .replace("javascript:", "")
        .replace("onerror=", "")
        .replace("onload=", "")
}

fn render_markdown(md: &str) -> String {
    use pulldown_cmark::{html, Options, Parser};
    let mut options = Options::empty();
    options.insert(Options::ENABLE_TABLES);
    options.insert(Options::ENABLE_STRIKETHROUGH);
    options.insert(Options::ENABLE_TASKLISTS);
    let parser = Parser::new_ext(md, options);
    let mut html_output = String::new();
    html::push_html(&mut html_output, parser);
    sanitize_html(&html_output)
}

async fn count_scalar(pool: &SqlitePool, query: &str) -> i64 {
    sqlx::query_scalar::<_, i64>(query)
        .fetch_one(pool)
        .await
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

pub async fn dashboard_stats(State(state): State<AppState>) -> impl IntoResponse {
    let pool = &state.db;

    let total_packages = count_scalar(pool, "SELECT COUNT(*) FROM packages").await;
    let total_versions = count_scalar(pool, "SELECT COUNT(*) FROM versions").await;
    let total_downloads = count_scalar(pool, "SELECT COUNT(*) FROM downloads").await;
    let total_repos = count_scalar(pool, "SELECT COUNT(*) FROM repositories").await;

    let recent = sqlx::query_as::<_, (String, String, String)>(
        "SELECT p.name, v.version, v.published_at
         FROM versions v
         JOIN packages p ON p.id = v.package_id
         ORDER BY v.published_at DESC
         LIMIT 10",
    )
    .fetch_all(pool)
    .await
    .unwrap_or_default();

    let recent_versions: Vec<RecentVersionResponse> = recent
        .into_iter()
        .map(|(package_name, version, published_at)| RecentVersionResponse {
            package_name,
            version,
            published_at,
        })
        .collect();

    Json(DashboardResponse {
        total_packages,
        total_versions,
        total_downloads,
        total_repos,
        recent_versions,
    })
}

pub async fn list_repositories(State(state): State<AppState>) -> impl IntoResponse {
    let pool = &state.db;

    let repos = sqlx::query_as::<_, (String,)>("SELECT name FROM repositories ORDER BY name")
        .fetch_all(pool)
        .await
        .unwrap_or_default();

    let repositories: Vec<RepoResponse> = repos
        .into_iter()
        .map(|(name,)| RepoResponse { name })
        .collect();

    Json(RepositoriesResponse { repositories })
}

pub async fn list_packages(
    State(state): State<AppState>,
    Query(params): Query<PackagesQuery>,
) -> impl IntoResponse {
    let pool = &state.db;
    let page = if params.page < 1 { 1 } else { params.page };
    let offset = (page - 1) * PAGE_SIZE;

    let (pkg_rows, count) = if !params.repo.is_empty() && !params.q.is_empty() {
        let search = format!("%{}%", params.q);
        let rows = sqlx::query_as::<_, (i64, String, Option<String>, String)>(
            "SELECT p.id, p.name, p.description, p.updated_at
             FROM packages p
             JOIN repositories r ON r.id = p.repository_id
             WHERE r.name = ?1 AND p.name LIKE ?2
             ORDER BY p.updated_at DESC
             LIMIT ?3 OFFSET ?4",
        )
        .bind(&params.repo)
        .bind(&search)
        .bind(PAGE_SIZE)
        .bind(offset)
        .fetch_all(pool)
        .await
        .unwrap_or_default();

        let cnt = sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM packages p
             JOIN repositories r ON r.id = p.repository_id
             WHERE r.name = ?1 AND p.name LIKE ?2",
        )
        .bind(&params.repo)
        .bind(&search)
        .fetch_one(pool)
        .await
        .unwrap_or(0);

        (rows, cnt)
    } else if !params.repo.is_empty() {
        let rows = sqlx::query_as::<_, (i64, String, Option<String>, String)>(
            "SELECT p.id, p.name, p.description, p.updated_at
             FROM packages p
             JOIN repositories r ON r.id = p.repository_id
             WHERE r.name = ?1
             ORDER BY p.updated_at DESC
             LIMIT ?2 OFFSET ?3",
        )
        .bind(&params.repo)
        .bind(PAGE_SIZE)
        .bind(offset)
        .fetch_all(pool)
        .await
        .unwrap_or_default();

        let cnt = sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM packages p
             JOIN repositories r ON r.id = p.repository_id
             WHERE r.name = ?1",
        )
        .bind(&params.repo)
        .fetch_one(pool)
        .await
        .unwrap_or(0);

        (rows, cnt)
    } else if !params.q.is_empty() {
        let search = format!("%{}%", params.q);
        let rows = sqlx::query_as::<_, (i64, String, Option<String>, String)>(
            "SELECT p.id, p.name, p.description, p.updated_at
             FROM packages p
             WHERE p.name LIKE ?1
             ORDER BY p.updated_at DESC
             LIMIT ?2 OFFSET ?3",
        )
        .bind(&search)
        .bind(PAGE_SIZE)
        .bind(offset)
        .fetch_all(pool)
        .await
        .unwrap_or_default();

        let cnt = sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM packages p WHERE p.name LIKE ?1",
        )
        .bind(&search)
        .fetch_one(pool)
        .await
        .unwrap_or(0);

        (rows, cnt)
    } else {
        let rows = sqlx::query_as::<_, (i64, String, Option<String>, String)>(
            "SELECT p.id, p.name, p.description, p.updated_at
             FROM packages p
             ORDER BY p.updated_at DESC
             LIMIT ?1 OFFSET ?2",
        )
        .bind(PAGE_SIZE)
        .bind(offset)
        .fetch_all(pool)
        .await
        .unwrap_or_default();

        let cnt = sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM packages")
            .fetch_one(pool)
            .await
            .unwrap_or(0);

        (rows, cnt)
    };

    let mut packages = Vec::with_capacity(pkg_rows.len());
    for (pkg_id, name, description, updated_at) in pkg_rows {
        let latest = sqlx::query_scalar::<_, String>(
            "SELECT version FROM versions WHERE package_id = ?1 ORDER BY id DESC LIMIT 1",
        )
        .bind(pkg_id)
        .fetch_optional(pool)
        .await
        .unwrap_or(None)
        .unwrap_or_else(|| "-".to_string());

        let downloads = sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM downloads d
             JOIN versions v ON v.id = d.version_id
             WHERE v.package_id = ?1",
        )
        .bind(pkg_id)
        .fetch_one(pool)
        .await
        .unwrap_or(0);

        packages.push(PackageResponse {
            name,
            latest_version: latest,
            description: description.unwrap_or_default(),
            downloads,
            published_at: updated_at,
        });
    }

    let has_next = (offset + PAGE_SIZE) < count;

    Json(PackagesResponse {
        packages,
        total: count,
        page,
        page_size: PAGE_SIZE,
        has_next,
    })
}

pub async fn package_detail(
    State(state): State<AppState>,
    axum::extract::Path(path): axum::extract::Path<String>,
) -> impl IntoResponse {
    let pool = &state.db;
    let pkg_name = path.as_str();

    let pkg = sqlx::query_as::<_, (i64, String, Option<String>, Option<String>, Option<String>)>(
        "SELECT id, name, description, readme, license FROM packages WHERE name = ?1 LIMIT 1",
    )
    .bind(pkg_name)
    .fetch_optional(pool)
    .await;

    let (pkg_id, name, description, readme, license) = match pkg {
        Ok(Some(row)) => row,
        Ok(None) => {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({"error": "package not found"})),
            )
                .into_response();
        }
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": e.to_string()})),
            )
                .into_response();
        }
    };

    let version_rows = sqlx::query_as::<_, (String, i64, String)>(
        "SELECT version, size, published_at FROM versions WHERE package_id = ?1 ORDER BY id DESC",
    )
    .bind(pkg_id)
    .fetch_all(pool)
    .await
    .unwrap_or_default();

    let versions: Vec<VersionResponse> = version_rows
        .into_iter()
        .map(|(version, size, published_at)| VersionResponse {
            version,
            size_display: format_size(size),
            published_at,
        })
        .collect();

    let dt_rows = sqlx::query_as::<_, (String, String)>(
        "SELECT dt.tag, v.version
         FROM dist_tags dt
         JOIN versions v ON v.id = dt.version_id
         WHERE dt.package_id = ?1",
    )
    .bind(pkg_id)
    .fetch_all(pool)
    .await
    .unwrap_or_default();

    let dist_tags: Vec<DistTagResponse> = dt_rows
        .into_iter()
        .map(|(tag, version)| DistTagResponse { tag, version })
        .collect();

    let total_downloads = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM downloads d
         JOIN versions v ON v.id = d.version_id
         WHERE v.package_id = ?1",
    )
    .bind(pkg_id)
    .fetch_one(pool)
    .await
    .unwrap_or(0);

    let readme_html = readme
        .as_deref()
        .map(render_markdown)
        .unwrap_or_default();

    Json(PackageDetailResponse {
        name,
        description: description.unwrap_or_default(),
        license: license.unwrap_or_default(),
        readme_html,
        total_downloads,
        versions,
        dist_tags,
    })
    .into_response()
}

pub async fn search(
    State(state): State<AppState>,
    Query(params): Query<SearchQuery>,
) -> impl IntoResponse {
    let pool = &state.db;

    let results = if !params.q.is_empty() {
        // Try FTS5 first, fallback to LIKE
        let fts_query = params.q
            .split_whitespace()
            .map(|word| format!("\"{}\"", word.replace('"', "")))
            .collect::<Vec<_>>()
            .join(" ");

        let fts_result = sqlx::query_as::<_, (i64, String, Option<String>)>(
            "SELECT p.id, p.name, p.description
             FROM packages p
             JOIN packages_fts fts ON p.id = fts.rowid
             WHERE packages_fts MATCH ?1
             ORDER BY rank
             LIMIT 50",
        )
        .bind(&fts_query)
        .fetch_all(pool)
        .await;

        let rows = match fts_result {
            Ok(r) => r,
            Err(_) => {
                // Fallback to LIKE
                let search = format!("%{}%", params.q);
                sqlx::query_as::<_, (i64, String, Option<String>)>(
                    "SELECT p.id, p.name, p.description
                     FROM packages p
                     WHERE p.name LIKE ?1 OR p.description LIKE ?1
                     ORDER BY p.name
                     LIMIT 50",
                )
                .bind(&search)
                .fetch_all(pool)
                .await
                .unwrap_or_default()
            }
        };

        let mut results = Vec::with_capacity(rows.len());
        for (pkg_id, name, description) in rows {
            let latest = sqlx::query_scalar::<_, String>(
                "SELECT version FROM versions WHERE package_id = ?1 ORDER BY id DESC LIMIT 1",
            )
            .bind(pkg_id)
            .fetch_optional(pool)
            .await
            .unwrap_or(None)
            .unwrap_or_else(|| "-".to_string());

            results.push(SearchResultResponse {
                name,
                latest_version: latest,
                description: description.unwrap_or_default(),
            });
        }
        results
    } else {
        Vec::new()
    };

    Json(SearchResponse {
        query: params.q,
        results,
    })
}
