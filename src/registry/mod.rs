pub mod cargo;
pub mod go;
pub mod npm;
pub mod oci;

use std::collections::HashMap;

use crate::auth::middleware::AuthUser;
use crate::auth::permissions::check_repo_permission;
use crate::db::Repository;
use crate::error::{AppError, AppResult};
use crate::server::AppState;

/// Extract the full package name from path parameters.
/// Scoped: scope="trace", name="httpclient" -> "@trace/httpclient".
/// Unscoped: name="react" -> "react".
/// Single source of truth for the copy that previously lived in the npm
/// handler and in the promote/deps/vulns API modules.
pub fn extract_package_name(params: &HashMap<String, String>) -> String {
    match params.get("scope") {
        Some(scope) => format!("@{}/{}", scope, params.get("name").unwrap_or(&String::new())),
        None => params.get("name").cloned().unwrap_or_default(),
    }
}

/// Enforce read access on a repository before serving any of its content.
///
/// - **Public** repositories are readable by anyone. Anonymous access still
///   depends on the global `anonymous_read` gate, which the auth middleware
///   enforces before the request reaches the handler.
/// - **Private** repositories require an authenticated caller that holds the
///   `read` permission on the repo (admin role, a matching `user_permissions`
///   grant, or the reader/publisher role default).
///
/// This closes the gap where read handlers served private repositories to
/// anyone, because `check_repo_permission` was only ever called for writes.
pub async fn ensure_can_read(
    db: &sqlx::SqlitePool,
    repo: &Repository,
    auth_user: Option<&AuthUser>,
) -> AppResult<()> {
    if repo.visibility == "public" {
        return Ok(());
    }
    match auth_user {
        Some(user) => {
            if check_repo_permission(db, user.user_id, &user.role, repo.id, "read").await? {
                Ok(())
            } else {
                Err(AppError::Forbidden(format!(
                    "read access denied on repository '{}'",
                    repo.name
                )))
            }
        }
        None => Err(AppError::Unauthorized(
            "authentication required to read this repository".to_string(),
        )),
    }
}

/// Enforce write (publish) access on a repository, with an actionable error
/// message when denied. The previous generic "insufficient permissions" did not
/// tell the caller that their role — typically the default `reader` — lacks
/// write, which made "I generated a token but can't publish" hard to diagnose.
pub async fn ensure_can_write(
    db: &sqlx::SqlitePool,
    repo: &Repository,
    auth_user: &AuthUser,
) -> AppResult<()> {
    if check_repo_permission(db, auth_user.user_id, &auth_user.role, repo.id, "write").await? {
        Ok(())
    } else {
        Err(AppError::Forbidden(format!(
            "write access denied on repository '{}': your role is '{}'. Publishing requires \
             the 'publisher' or 'admin' role, or an explicit write permission on this \
             repository granted by an admin.",
            repo.name, auth_user.role
        )))
    }
}

/// Ensure the repository's declared format matches the protocol being used.
/// Without this guard a payload of one format could be published into a repo of
/// another (e.g. an npm tarball into a `cargo` repo), silently corrupting it
/// since the underlying tables are shared.
pub fn ensure_format(repo: &Repository, expected: &str) -> AppResult<()> {
    if repo.format == expected {
        Ok(())
    } else {
        Err(AppError::BadRequest(format!(
            "repository '{}' is a '{}' repository, not '{}'",
            repo.name, repo.format, expected
        )))
    }
}

/// Ensure the repository is a `hosted` one before accepting a publish/push.
/// Factored out of the per-format publish handlers where the check was
/// duplicated verbatim.
pub fn ensure_hosted(repo: &Repository) -> AppResult<()> {
    if repo.repo_type == "hosted" {
        Ok(())
    } else {
        Err(AppError::BadRequest(
            "can only publish to hosted repositories".to_string(),
        ))
    }
}

// ---------------------------------------------------------------------------
// Package name / version validation
// ---------------------------------------------------------------------------
//
// Package names and versions come straight from the URL or the request body
// and end up interpolated into storage paths (`npm/{repo}/{name}/...`,
// `cargo/...`, `go/...`). `safe_path` blocks escapes from the storage root,
// but without validation a name containing `/` still creates an arbitrary
// directory tree, pollutes the DB, and produces unresolvable packages. These
// validators enforce per-ecosystem naming rules at the publish boundary; all
// failures map to 400 BadRequest.

/// Unscoped npm name part (also used for the scope and the name of a scoped
/// package): lowercase `[a-z0-9._-]+`, must not start with `.` or `_`.
fn is_valid_npm_name_part(part: &str) -> bool {
    !part.is_empty()
        && !part.starts_with('.')
        && !part.starts_with('_')
        && part
            .bytes()
            .all(|b| matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'.' | b'_' | b'-'))
}

/// A Go module path segment: `[a-zA-Z0-9._~-]+`, and neither `.` nor `..`.
fn is_valid_go_segment(segment: &str) -> bool {
    !segment.is_empty()
        && segment != "."
        && segment != ".."
        && segment.bytes().all(|b| {
            matches!(b, b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'.' | b'_' | b'~' | b'-')
        })
}

/// An OCI repository-name segment per the distribution spec:
/// `[a-z0-9]+(?:[._-][a-z0-9]+)*` — alphanumeric runs joined by single
/// separators, never leading/trailing or doubled.
fn is_valid_oci_segment(segment: &str) -> bool {
    let bytes = segment.as_bytes();
    if bytes.is_empty() {
        return false;
    }
    let is_alnum = |b: u8| matches!(b, b'a'..=b'z' | b'0'..=b'9');
    if !is_alnum(bytes[0]) || !is_alnum(bytes[bytes.len() - 1]) {
        return false;
    }
    let mut prev_was_separator = false;
    for &b in bytes {
        if is_alnum(b) {
            prev_was_separator = false;
        } else if matches!(b, b'.' | b'_' | b'-') {
            if prev_was_separator {
                return false;
            }
            prev_was_separator = true;
        } else {
            return false;
        }
    }
    true
}

/// Validate a package name for the given ecosystem (`npm`, `cargo`, `go`,
/// `oci`). Called at the top of every publish handler so hostile names (path
/// separators, traversal sequences, forbidden characters) are rejected with a
/// 400 before touching the DB or storage.
pub fn validate_package_name(format: &str, name: &str) -> AppResult<()> {
    let invalid =
        || AppError::BadRequest(format!("invalid {format} package name: '{name}'"));

    match format {
        "npm" => {
            // Unscoped: [a-z0-9._-]+ ; scoped: @scope/name with exactly one
            // '/', both parts following the unscoped rule. Max 214 chars total
            // (npm registry limit), no leading '.' or '_'.
            if name.len() > 214 {
                return Err(invalid());
            }
            match name.strip_prefix('@') {
                Some(rest) => {
                    let (scope, pkg) = rest.split_once('/').ok_or_else(invalid)?;
                    if pkg.contains('/')
                        || !is_valid_npm_name_part(scope)
                        || !is_valid_npm_name_part(pkg)
                    {
                        return Err(invalid());
                    }
                }
                None => {
                    if !is_valid_npm_name_part(name) {
                        return Err(invalid());
                    }
                }
            }
        }
        "cargo" => {
            // [a-zA-Z][a-zA-Z0-9_-]*, max 64 chars (crates.io rules).
            if name.is_empty() || name.len() > 64 {
                return Err(invalid());
            }
            let mut bytes = name.bytes();
            if !bytes.next().is_some_and(|b| b.is_ascii_alphabetic()) {
                return Err(invalid());
            }
            if !bytes.all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-') {
                return Err(invalid());
            }
        }
        "go" => {
            // Module path: non-empty segments of [a-zA-Z0-9._~-] separated by
            // '/', no '.'/'..' segments. Capped at 255 chars to keep the
            // user-controlled DB/storage key bounded.
            if name.is_empty() || name.len() > 255 {
                return Err(invalid());
            }
            if !name.split('/').all(is_valid_go_segment) {
                return Err(invalid());
            }
        }
        "oci" => {
            // Distribution-spec repository name: '/'-separated segments, each
            // [a-z0-9]+(?:[._-][a-z0-9]+)*. Capped at 255 chars (spec limit).
            if name.is_empty() || name.len() > 255 {
                return Err(invalid());
            }
            if !name.split('/').all(is_valid_oci_segment) {
                return Err(invalid());
            }
        }
        other => {
            return Err(AppError::BadRequest(format!(
                "unsupported package format: {other}"
            )));
        }
    }
    Ok(())
}

/// Validate a version string: `[a-zA-Z0-9.+-]+`, at most 128 chars. Covers
/// semver (npm/cargo) and Go pseudo-versions; blocks path separators and
/// traversal sequences in storage paths like `{name}-{version}.crate`.
pub fn validate_version(version: &str) -> AppResult<()> {
    if version.is_empty()
        || version.len() > 128
        || !version
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'+' | b'-'))
    {
        return Err(AppError::BadRequest(format!(
            "invalid version: '{version}'"
        )));
    }
    Ok(())
}

/// Validate an OCI tag per the distribution spec:
/// `[a-zA-Z0-9_][a-zA-Z0-9._-]{0,127}`. Kept separate from
/// [`validate_version`] because legal docker tags may contain `_` and
/// uppercase, which the generic version rule rejects.
pub fn validate_oci_tag(tag: &str) -> AppResult<()> {
    let invalid = || AppError::BadRequest(format!("invalid OCI tag: '{tag}'"));
    let bytes = tag.as_bytes();
    if bytes.is_empty() || bytes.len() > 128 {
        return Err(invalid());
    }
    if !(bytes[0].is_ascii_alphanumeric() || bytes[0] == b'_') {
        return Err(invalid());
    }
    if !bytes
        .iter()
        .all(|&b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-'))
    {
        return Err(invalid());
    }
    Ok(())
}

/// Shared post-publish side effects, factored out of the per-format publish
/// handlers where they were duplicated verbatim: fire the `package.published`
/// webhook, emit the real-time event, then run the vulnerability scan. With
/// `block_on_critical`, a critical finding aborts the publish (returns Err);
/// otherwise the scan runs in the background.
///
/// `version_id` is `None` for ecosystems that do not record rows in the
/// `versions` table (OCI stores manifests/tags in dedicated tables); the scan
/// step is skipped in that case since `vulnerability_scans.version_id` has a
/// foreign key on `versions(id)`.
#[allow(clippy::too_many_arguments)]
pub async fn finalize_publish(
    state: &AppState,
    ecosystem: &str,
    repo_name: &str,
    package_name: &str,
    version_str: &str,
    version_id: Option<i64>,
    metadata_json: &str,
    published_by: &str,
) -> AppResult<()> {
    state
        .webhook_dispatcher
        .dispatch(
            "package.published",
            &serde_json::json!({
                "package": package_name,
                "version": version_str,
                "repository": repo_name,
                "published_by": published_by,
            }),
        )
        .await;

    emit_package_event(
        state,
        "package.published",
        repo_name,
        serde_json::json!({
            "package": package_name,
            "version": version_str,
            "repository": repo_name,
            "format": ecosystem,
            "published_by": published_by,
        }),
    )
    .await;

    let Some(version_id) = version_id else {
        return Ok(());
    };

    if state.vuln_scan_config.block_on_critical {
        let scan_result = state
            .vuln_scanner
            .scan_version(&state.db, version_id, metadata_json, ecosystem)
            .await;
        if let Ok(ref result) = scan_result {
            if result.status == "critical" {
                return Err(AppError::BadRequest(
                    "publish blocked: critical vulnerabilities found in dependencies".to_string(),
                ));
            }
        }
    } else {
        let scanner = state.vuln_scanner.clone();
        let db = state.db.clone();
        let meta_json = metadata_json.to_string();
        let eco = ecosystem.to_string();
        tokio::spawn(async move {
            if let Err(e) = scanner.scan_version(&db, version_id, &meta_json, &eco).await {
                tracing::warn!(error = %e, "Background vulnerability scan failed");
            }
        });
    }
    Ok(())
}

/// Broadcast a package event on the real-time bus, scoped by the repository's
/// visibility:
///
/// - **public repo** — full payload, visible to everyone (incl. anonymous);
/// - **private repo** — full payload for admins only, plus an anonymized
///   `registry.changed` hint for authenticated users so their views refetch
///   without leaking private package names to users lacking read access.
pub async fn emit_package_event(
    state: &AppState,
    event_type: &str,
    repo_name: &str,
    payload: serde_json::Value,
) {
    use crate::events::Visibility;

    let is_public = matches!(
        crate::db::get_repository_by_name(&state.db, repo_name).await,
        Ok(Some(ref repo)) if repo.visibility == "public"
    );

    if is_public {
        state.events.emit(event_type, Visibility::Public, payload);
    } else {
        state.events.emit(event_type, Visibility::Admin, payload);
        state.events.emit(
            "registry.changed",
            Visibility::Authenticated,
            serde_json::json!({ "repository": repo_name }),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::{validate_oci_tag, validate_package_name, validate_version};

    #[test]
    fn npm_names() {
        for ok in [
            "react",
            "lodash.merge",
            "my-pkg_2",
            "@scope/pkg",
            "@my.scope/my-pkg_x",
        ] {
            assert!(validate_package_name("npm", ok).is_ok(), "{ok} should be valid");
        }
        for bad in [
            "",
            "React",          // uppercase
            ".dotfirst",      // leading dot
            "_underfirst",    // leading underscore
            "a/b",            // unscoped with slash -> arbitrary storage tree
            "../evil",        // traversal
            "@scope",         // scoped without name
            "@scope/a/b",     // two slashes
            "@/pkg",          // empty scope
            "@scope/",        // empty name
            "@scope/.dot",    // leading dot in name part
            "@Scope/pkg",     // uppercase scope
            "a b",            // space
            &"x".repeat(215), // too long
        ] {
            assert!(validate_package_name("npm", bad).is_err(), "{bad} should be rejected");
        }
    }

    #[test]
    fn cargo_names() {
        for ok in ["serde", "X9", "a-b_c123", &"a".repeat(64)] {
            assert!(validate_package_name("cargo", ok).is_ok(), "{ok} should be valid");
        }
        for bad in [
            "",
            "1abc",           // must start with a letter
            "-abc",
            "_abc",
            "a.b",            // dots not allowed
            "a/b",
            "../evil",
            &"a".repeat(65),  // too long
        ] {
            assert!(validate_package_name("cargo", bad).is_err(), "{bad} should be rejected");
        }
    }

    #[test]
    fn go_names() {
        for ok in [
            "mymodule",
            "github.com/org/repo",
            "golang.org/x/tools",
            "example.com/Org_1/~repo-v2",
        ] {
            assert!(validate_package_name("go", ok).is_ok(), "{ok} should be valid");
        }
        for bad in [
            "",
            "a//b",           // empty segment
            "/a/b",           // leading slash -> empty segment
            "a/b/",           // trailing slash -> empty segment
            "a/../b",         // traversal segment
            "../evil",
            "./a",
            "a b/c",          // space
            "a!b",            // forbidden char
            &format!("a/{}", "b".repeat(255)), // too long
        ] {
            assert!(validate_package_name("go", bad).is_err(), "{bad} should be rejected");
        }
    }

    #[test]
    fn oci_names() {
        for ok in ["myapp", "my-app.v2", "a/b", "team1/backend_api"] {
            assert!(validate_package_name("oci", ok).is_ok(), "{ok} should be valid");
        }
        for bad in [
            "",
            "MyApp",          // uppercase
            "-lead",          // leading separator
            "trail-",         // trailing separator
            "a..b",           // doubled separator
            "a__b",           // doubled separator
            "a//b",           // empty segment
            "../evil",
            "a b",
        ] {
            assert!(validate_package_name("oci", bad).is_err(), "{bad} should be rejected");
        }
    }

    #[test]
    fn unknown_format_rejected() {
        assert!(validate_package_name("pypi", "anything").is_err());
    }

    #[test]
    fn versions() {
        for ok in ["1.0.0", "v1.2.3", "1.0.0-beta.1+build.5", "0.0.0-20230101120000-abcdef123456"] {
            assert!(validate_version(ok).is_ok(), "{ok} should be valid");
        }
        for bad in ["", "1.0.0/evil", "../1.0.0", "1.0 .0", "1_0", &"1".repeat(129)] {
            assert!(validate_version(bad).is_err(), "{bad} should be rejected");
        }
    }

    #[test]
    fn oci_tags() {
        for ok in ["latest", "v1.2.3", "main_build-42", "_internal", "V2"] {
            assert!(validate_oci_tag(ok).is_ok(), "{ok} should be valid");
        }
        for bad in ["", ".hidden", "-lead", "sha256:abc", "a/b", &"t".repeat(129)] {
            assert!(validate_oci_tag(bad).is_err(), "{bad} should be rejected");
        }
    }
}
