use axum::{
    extract::{Extension, Query, State},
    response::IntoResponse,
    Json,
};
use serde::{Deserialize, Serialize};

use crate::auth::middleware::AuthUser;
use crate::domain::Visibility;
use crate::error::{AppError, AppResult};
use crate::ports::dashboard::{PackageFilter, Reach};
use crate::ports::search::{SearchQuery as Tokens, SearchScope};
use crate::server::AppState;
use crate::wire::wire_ts;

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
    #[serde(rename = "type")]
    repo_type: String,
    format: String,
    visibility: Visibility,
    upstream: Option<String>,
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
const RECENT: i64 = 10;
const SEARCH_RESULTS: u32 = 50;

/// What a package has been released as when no version row claims it: a
/// package created by a publish that then failed.
const NO_VERSION: &str = "-";

type Caller = Option<Extension<AuthUser>>;

/// Which packages a caller may count, list and search. Admins see every
/// repository; everyone else, authenticated or not, sees the public ones.
fn package_reach(caller: &Caller) -> Reach {
    match caller.as_ref().map(|user| crate::api::admin_standing(&user.0)) {
        Some(true) => Reach::Everything,
        _ => Reach::PublicOnly,
    }
}

/// Repositories answer to a different caller, and always have: any
/// authenticated user sees the whole list, an anonymous one the public part.
/// Before that filter the endpoint leaked private repository names to anyone
/// whenever `anonymous_read` was on.
fn repository_reach(caller: &Caller) -> Reach {
    match caller {
        Some(_) => Reach::Everything,
        None => Reach::PublicOnly,
    }
}

fn format_size(bytes: i64) -> String {
    if bytes < 1024 {
        format!("{} B", bytes)
    } else if bytes < 1024 * 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    }
}

fn render_markdown(md: &str) -> String {
    use pulldown_cmark::{html, Options, Parser};
    let mut options = Options::empty();
    options.insert(Options::ENABLE_TABLES);
    options.insert(Options::ENABLE_STRIKETHROUGH);
    options.insert(Options::ENABLE_TASKLISTS);
    let parser = Parser::new_ext(md, options);
    let mut html_output = String::new();
    // Allowlist-based HTML sanitization via ammonia, replacing the previous
    // hand-rolled blocklist which was trivially bypassable (stored XSS in
    // package READMEs, e.g. `<img src=x onmouseover=...>`).
    html::push_html(&mut html_output, parser);
    ammonia::clean(&html_output)
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

pub async fn dashboard_stats(
    State(state): State<AppState>,
    caller: Caller,
) -> AppResult<impl IntoResponse> {
    let reach = package_reach(&caller);
    let totals = state.dashboard.totals(reach).await?;
    let total_repos = state
        .dashboard
        .repository_count(repository_reach(&caller))
        .await?;
    let recent = state.dashboard.recent_versions(reach, RECENT).await?;

    Ok(Json(DashboardResponse {
        total_packages: totals.packages,
        total_versions: totals.versions,
        total_downloads: totals.downloads,
        total_repos,
        recent_versions: recent
            .into_iter()
            .map(|line| RecentVersionResponse {
                package_name: line.package,
                version: line.version,
                published_at: wire_ts(line.published_at),
            })
            .collect(),
    }))
}

pub async fn list_repositories(
    State(state): State<AppState>,
    caller: Caller,
) -> AppResult<impl IntoResponse> {
    let reach = repository_reach(&caller);
    // Upstream URLs stay behind authentication: they can carry credentials in
    // userinfo and reveal internal mirror hosts.
    let authenticated = reach == Reach::Everything;

    let repositories = state
        .repos
        .all()
        .await?
        .into_iter()
        .filter(|repo| authenticated || repo.visibility == Visibility::Public)
        .map(|repo| RepoResponse {
            name: repo.name,
            repo_type: repo.repo_type,
            format: repo.format,
            visibility: repo.visibility,
            upstream: authenticated.then_some(repo.upstream_url).flatten(),
        })
        .collect();

    Ok(Json(RepositoriesResponse { repositories }))
}

pub async fn list_packages(
    State(state): State<AppState>,
    Query(params): Query<PackagesQuery>,
    caller: Caller,
) -> AppResult<impl IntoResponse> {
    let page = params.page.max(1);
    // saturating_* so a huge `page` cannot overflow the i64 multiplication
    // (panics in debug, wraps to a negative OFFSET in release).
    let offset = page.saturating_sub(1).saturating_mul(PAGE_SIZE);

    let found = state
        .dashboard
        .packages(&PackageFilter {
            reach: package_reach(&caller),
            repository: some_text(&params.repo),
            name_contains: some_text(&params.q),
            limit: PAGE_SIZE,
            offset,
        })
        .await?;

    Ok(Json(PackagesResponse {
        packages: found
            .packages
            .into_iter()
            .map(|pkg| PackageResponse {
                name: pkg.name,
                latest_version: pkg.latest_version.unwrap_or_else(|| NO_VERSION.to_string()),
                description: pkg.description.unwrap_or_default(),
                downloads: pkg.downloads,
                published_at: wire_ts(pkg.updated_at),
            })
            .collect(),
        total: found.total,
        page,
        page_size: PAGE_SIZE,
        has_next: offset.saturating_add(PAGE_SIZE) < found.total,
    }))
}

pub async fn package_detail(
    State(state): State<AppState>,
    axum::extract::Path(path): axum::extract::Path<String>,
    caller: Caller,
) -> AppResult<impl IntoResponse> {
    let detail = state
        .dashboard
        .package_detail(&path, package_reach(&caller))
        .await?
        .ok_or_else(|| AppError::NotFound("package not found".to_string()))?;

    Ok(Json(PackageDetailResponse {
        name: detail.package.name,
        description: detail.package.description.unwrap_or_default(),
        license: detail.package.license.unwrap_or_default(),
        readme_html: detail.package.readme.as_deref().map(render_markdown).unwrap_or_default(),
        total_downloads: detail.total_downloads,
        versions: detail
            .versions
            .into_iter()
            .map(|version| VersionResponse {
                version: version.version,
                size_display: format_size(version.size),
                published_at: wire_ts(version.published_at),
            })
            .collect(),
        dist_tags: detail
            .dist_tags
            .into_iter()
            .map(|tagged| DistTagResponse {
                tag: tagged.tag,
                version: tagged.version,
            })
            .collect(),
    }))
}

pub async fn search(
    State(state): State<AppState>,
    Query(params): Query<SearchQuery>,
    caller: Caller,
) -> AppResult<impl IntoResponse> {
    // No query at all, and a query that sanitises to no token, are the same
    // answer here: the panel searches, it never browses, and an expression
    // FTS5 would refuse never reaches the index.
    let results = match Tokens::parse(&params.q) {
        Some(query) => hits(&state, &query, package_reach(&caller)).await?,
        None => Vec::new(),
    };

    Ok(Json(SearchResponse {
        query: params.q,
        results,
    }))
}

/// The search panel's rows: the index answers with packages, and what each
/// was last released as is one more call, not one per row.
async fn hits(
    state: &AppState,
    query: &Tokens,
    reach: Reach,
) -> AppResult<Vec<SearchResultResponse>> {
    let scope = match reach {
        Reach::Everything => SearchScope::All,
        Reach::PublicOnly => SearchScope::PublicOnly,
    };
    let found = state.search.search(scope, Some(query), SEARCH_RESULTS).await?;
    let latest = state
        .dashboard
        .latest_versions(&found.iter().map(|pkg| pkg.id).collect::<Vec<_>>())
        .await?;

    Ok(found
        .into_iter()
        .map(|pkg| SearchResultResponse {
            latest_version: latest
                .get(&pkg.id)
                .cloned()
                .unwrap_or_else(|| NO_VERSION.to_string()),
            name: pkg.name,
            description: pkg.description.unwrap_or_default(),
        })
        .collect())
}

/// An absent filter box and an empty one are the same request.
fn some_text(raw: &str) -> Option<&str> {
    (!raw.is_empty()).then_some(raw)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn caller(role: &str) -> Caller {
        Some(Extension(AuthUser {
            token: String::new(),
            user_id: Some(1),
            username: "u".to_string(),
            role: role.to_string(),
            must_change_password: false,
            token_name: None,
            api_token_id: None,
            scope: crate::domain::TokenScope::Inherit,
        }))
    }

    /// The two rules the panels used to splice into their SQL, which differ:
    /// a plain user counts every repository but only public packages.
    #[test]
    fn a_caller_reaches_repositories_and_packages_by_different_rules() {
        assert_eq!(package_reach(&caller("admin")), Reach::Everything);
        assert_eq!(package_reach(&caller("user")), Reach::PublicOnly);
        assert_eq!(package_reach(&None), Reach::PublicOnly);

        assert_eq!(repository_reach(&caller("admin")), Reach::Everything);
        assert_eq!(repository_reach(&caller("user")), Reach::Everything);
        assert_eq!(repository_reach(&None), Reach::PublicOnly);
    }

    #[test]
    fn an_empty_filter_box_is_no_filter() {
        assert_eq!(some_text(""), None);
        assert_eq!(some_text("left-pad"), Some("left-pad"));
    }

    /// The panel's deleted `LIKE` fallback used to absorb these; now they
    /// never reach the index, which is the same empty answer without the 500.
    #[test]
    fn a_query_that_sanitises_away_asks_the_index_nothing() {
        assert!(Tokens::parse(" ").is_none());
        assert!(Tokens::parse("\"").is_none());
        assert!(Tokens::parse("left-pad").is_some());
    }
}
