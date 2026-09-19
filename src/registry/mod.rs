pub mod cargo;
pub mod go;
pub mod maven;
pub mod mcp;
pub mod npm;
pub mod archive;
pub mod nuget;
pub mod oci;
pub mod pypi;
pub mod raw;
pub mod resolve;
pub mod routing;
pub mod rules;

use std::collections::HashMap;

use crate::app::authorize::Authorize;
use crate::auth::middleware::AuthUser;
use crate::auth::publish_limit::Admission;
use crate::domain::{Format, RepoAction, RepoKind, Repository, UrlRepo};
use crate::error::{AppError, AppResult};
use crate::ports::repositories::RepositoryStore;
use crate::registry::resolve::Cx;
use crate::server::AppState;

/// Extract the full package name from path parameters.
/// Scoped: scope="acme", name="httpclient" -> "@acme/httpclient".
/// Unscoped: name="react" -> "react".
/// Single source of truth for the copy that previously lived in the npm
/// handler and in the promote/deps/vulns API modules.
pub fn extract_package_name(params: &HashMap<String, String>) -> String {
    match params.get("scope") {
        Some(scope) => format!("@{}/{}", scope, params.get("name").unwrap_or(&String::new())),
        None => params.get("name").cloned().unwrap_or_default(),
    }
}

/// The request's resolver context: the one place the composition root's
/// state is projected onto the ports a leaf may reach, so no leaf and no
/// format module ever holds the whole of it.
pub fn cx<'a>(state: &'a AppState, auth: Option<&'a AuthUser>, repo: &'a Repository) -> Cx<'a> {
    Cx {
        repos: state.repos.as_ref(),
        perms: state.permissions.as_ref(),
        packages: state.packages.as_ref(),
        oci: state.oci.as_ref(),
        maven: state.maven.as_ref(),
        raw: state.raw.as_ref(),
        search: state.search.as_ref(),
        cached: state.cached.as_ref(),
        nuget: state.nuget_feed.as_ref(),
        proxy: &state.proxy,
        policy: &state.policy,
        creds: state.upstream_auth.as_ref(),
        auth,
        anonymous_read: state.auth.anonymous_read,
        routing: state.routing.as_ref(),
        refusals: state.refusals.as_ref(),
        url: UrlRepo(&repo.name),
        base_url: &state.base_url,
    }
}

/// Load a repository by name; a missing one is a 404 naming it.
pub async fn load_repo(repos: &dyn RepositoryStore, name: &str) -> AppResult<Repository> {
    repos
        .by_name(name)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("repository not found: {name}")))
}

/// Enforce read access on a repository before serving any of its content.
///
/// The decision is `Authorize`'s: the ladder, then the presented credential's
/// scope, then the verdict that tells a 401 (nothing presented) from a 403
/// (what was presented is not enough).
pub async fn ensure_can_read(
    authz: &Authorize<'_>,
    repo: &Repository,
    auth_user: Option<&AuthUser>,
) -> AppResult<()> {
    authz
        .repository(auth_user, repo, RepoAction::Read)
        .await
        .into_result(
            || format!("read access denied on repository '{}'", repo.name),
            "authentication required to read this repository",
        )
}

/// Enforce write (publish) access on a repository, with an actionable error
/// message when denied. The previous generic "insufficient permissions" did not
/// tell the caller that their role — typically the default `reader` — lacks
/// write, which made "I generated a token but can't publish" hard to diagnose.
pub async fn ensure_can_write(
    authz: &Authorize<'_>,
    repo: &Repository,
    auth_user: &AuthUser,
) -> AppResult<()> {
    ensure_action(authz, repo, auth_user, RepoAction::Write).await
}

/// The verb a route really asks for. Deletion of content asks for `delete`,
/// which the ladder never asked for before scopes: a token allowed to publish
/// is not thereby allowed to remove.
pub async fn ensure_action(
    authz: &Authorize<'_>,
    repo: &Repository,
    auth_user: &AuthUser,
    action: RepoAction,
) -> AppResult<()> {
    authz
        .repository(Some(auth_user), repo, action)
        .await
        .into_result(
            || {
                format!(
                    "write access denied on repository '{}': your role is '{}'. Publishing \
                     requires the 'publisher' or 'admin' role, or an explicit write permission \
                     on this repository granted by an admin.",
                    repo.name, auth_user.role
                )
            },
            "authentication required to write to this repository",
        )
}

/// Enforce delete access, the matrix column no protocol used before raw: a
/// publisher may add a file without being able to remove one, and an admin
/// or an explicit grant may.
pub async fn ensure_can_delete(
    authz: &Authorize<'_>,
    repo: &Repository,
    auth_user: &AuthUser,
) -> AppResult<()> {
    ensure_action(authz, repo, auth_user, RepoAction::Delete).await
}

/// Ensure the repository's declared format matches the protocol being used.
/// Without this guard a payload of one format could be published into a repo of
/// another (e.g. an npm tarball into a `cargo` repo), silently corrupting it
/// since the underlying tables are shared.
pub fn ensure_format(repo: &Repository, expected: Format) -> AppResult<()> {
    let format = repo.fmt()?;
    if format == expected {
        Ok(())
    } else {
        Err(AppError::BadRequest(format!(
            "repository '{}' is a '{}' repository, not '{}'",
            repo.name,
            format.as_str(),
            expected.as_str()
        )))
    }
}

/// Count one publish against the account's configured limit, before the body
/// is read: a refusal costs the server nothing and tells the client the limit
/// it hit and when to come back.
pub fn meter_publish(
    state: &AppState,
    auth_user: &AuthUser,
    format: Format,
    repo_name: &str,
) -> AppResult<()> {
    let verdict = state
        .publish_meter
        .admit(&auth_user.username, format, repo_name, state.clock.now());
    match &verdict {
        Admission::Allowed => Ok(()),
        Admission::Refused {
            retry_after_secs, ..
        } => Err(AppError::RateLimited {
            message: verdict.message().unwrap_or_default(),
            retry_after_secs: *retry_after_secs,
        }),
    }
}

/// Ensure the repository is a `hosted` one before accepting a publish/push.
/// Factored out of the per-format publish handlers where the check was
/// duplicated verbatim.
pub fn ensure_hosted(repo: &Repository) -> AppResult<()> {
    if repo.kind()? == RepoKind::Hosted {
        Ok(())
    } else {
        Err(AppError::BadRequest(
            "can only publish to hosted repositories".to_string(),
        ))
    }
}

