use std::collections::{HashMap, HashSet};

use axum::{
    body::Body,
    extract::{Path, State},
    http::{header, HeaderMap, HeaderValue, Response, StatusCode, Uri},
    response::IntoResponse,
    Json,
};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tracing::warn;

use crate::auth::middleware::AuthUser;
use crate::domain::{Format, UrlRepo, Visibility};
use crate::error::{AppError, AppResult};
use crate::registry::cx;
use crate::registry::resolve::collect;
use crate::server::AppState;

use super::leaves::{IndexLeaf, IndexLines};
use super::{compute_prefix, line_field};

/// Readable without a token (the auth middleware lets it through): cargo
/// fetches it before knowing whether to authenticate and learns to from
/// `auth-required`. A sent token must still hold read; a tokenless caller
/// learns only that the repository exists and is a cargo one.
pub async fn config_json(
    State(state): State<AppState>,
    Path(repo_name): Path<String>,
    auth: Option<axum::Extension<AuthUser>>,
) -> AppResult<impl IntoResponse> {
    let repo = crate::registry::load_repo(state.repos.as_ref(), &repo_name).await?;
    crate::registry::ensure_format(&repo, Format::Cargo)?;
    let auth = auth.as_ref().map(|e| &e.0);
    if auth.is_some() {
        crate::registry::ensure_can_read(&state.authorize(), &repo, auth).await?;
    }
    let cx = cx(&state, auth, &repo);
    Ok(Json(config_body(
        cx.base_url,
        cx.url,
        repo.visibility != Visibility::Public,
    )))
}

/// Never proxied: `dl` and `api` name the repository the client addressed.
fn config_body(base_url: &str, url: UrlRepo<'_>, auth_required: bool) -> Value {
    let mut config = json!({
        "dl": format!("{base_url}/{}/api/v1/crates", url.0),
        "api": format!("{base_url}/{}", url.0),
    });
    if auth_required {
        config["auth-required"] = json!(true);
    }
    config
}

/// `GET /{repo}/index/{prefix..}/{name}` on the four prefix shapes of the
/// sparse protocol: the prefix must be the one cargo derives from the name.
pub async fn get_index_entry(
    State(state): State<AppState>,
    Path(params): Path<HashMap<String, String>>,
    uri: Uri,
    headers: HeaderMap,
    auth: Option<axum::Extension<AuthUser>>,
) -> AppResult<Response<Body>> {
    let (repo_name, name) = index_params(&params)?;
    crate::registry::rules::rules_of(crate::domain::Format::Cargo)?.validate(name)?;
    if index_prefix(uri.path()) != Some(compute_prefix(name)) {
        return Err(AppError::NotFound(format!("crate not found: {name}")));
    }
    let repo = crate::registry::load_repo(state.repos.as_ref(), repo_name).await?;
    let auth = auth.as_ref().map(|e| &e.0);
    crate::registry::ensure_can_read(&state.authorize(), &repo, auth).await?;

    let cx = cx(&state, auth, &repo);
    let leaf = IndexLeaf {
        name: name.to_string(),
    };
    let collected = collect(&cx, &repo, &leaf).await?;
    let merged = merge_index_lines(collected.hits);
    if merged.lines.is_empty() {
        return Err(match collected.degraded {
            Some(why) => AppError::BadGateway(format!("group {}: {why}", cx.url.0)),
            None => AppError::NotFound(format!("crate not found: {name}")),
        });
    }
    let body = merged.lines.join("\n");
    let etag = format!("\"{:x}\"", Sha256::digest(body.as_bytes()));
    let mut response = Response::builder().header(header::ETAG, &etag);
    if merged.stale {
        response = response.header(header::WARNING, "110 - \"Response is Stale\"");
    }
    if let Some(why) = collected.degraded {
        response = response.header(header::WARNING, degraded_warning(&why));
    }
    let build_failed = |e| AppError::Internal(format!("response build failed: {e}"));
    if if_none_match_hits(&headers, &etag) {
        return response
            .status(StatusCode::NOT_MODIFIED)
            .body(Body::empty())
            .map_err(build_failed);
    }
    response
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body))
        .map_err(build_failed)
}

fn index_params(params: &HashMap<String, String>) -> AppResult<(&str, &str)> {
    let capture = |key: &str| {
        params
            .get(key)
            .map(String::as_str)
            .ok_or_else(|| AppError::BadRequest(format!("missing {key}")))
    };
    Ok((capture("repo")?, capture("name")?))
}

/// The segments between `index` and the crate name, literal ones included:
/// `/{repo}/index/3/a/abc` -> `3/a`.
fn index_prefix(path: &str) -> Option<String> {
    let segments: Vec<&str> = path.trim_start_matches('/').split('/').collect();
    match segments.as_slice() {
        [_repo, "index", prefix @ .., _name] if !prefix.is_empty() => Some(prefix.join("/")),
        _ => None,
    }
}

/// Union in member order, one line per `vers`, the first member's line winning.
fn merge_index_lines(hits: Vec<IndexLines>) -> IndexLines {
    let mut seen = HashSet::new();
    let mut merged = IndexLines {
        lines: Vec::new(),
        stale: false,
    };
    for hit in hits {
        merged.stale |= hit.stale;
        for line in hit.lines {
            match line_field(&line, "vers") {
                Some(vers) => {
                    if seen.insert(vers) {
                        merged.lines.push(line);
                    }
                }
                None => warn!(line = %line, "index line without vers, dropped"),
            }
        }
    }
    merged
}

fn if_none_match_hits(headers: &HeaderMap, etag: &str) -> bool {
    headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.split(',').map(str::trim).any(|t| t == etag || t == "*"))
}

fn degraded_warning(why: &str) -> HeaderValue {
    let text: String = why
        .chars()
        .filter(|c| (c.is_ascii_graphic() || *c == ' ') && *c != '"')
        .collect();
    HeaderValue::from_str(&format!("199 - \"{text}\""))
        .unwrap_or_else(|_| HeaderValue::from_static("199 - \"member unavailable\""))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hit(stale: bool, lines: &[&str]) -> IndexLines {
        IndexLines {
            lines: lines.iter().map(|l| l.to_string()).collect(),
            stale,
        }
    }

    #[test]
    fn merge_dedups_by_vers_first_member_wins() {
        let merged = merge_index_lines(vec![
            hit(false, &[r#"{"vers":"0.2.0","cksum":"a"}"#]),
            hit(
                true,
                &[
                    r#"{"vers":"0.1.0","cksum":"b"}"#,
                    r#"{"vers":"0.2.0","cksum":"c"}"#,
                    "junk",
                ],
            ),
        ]);
        assert_eq!(
            merged.lines,
            vec![
                r#"{"vers":"0.2.0","cksum":"a"}"#,
                r#"{"vers":"0.1.0","cksum":"b"}"#
            ]
        );
        assert!(merged.stale);
    }

    #[test]
    fn degraded_warning_is_a_valid_header() {
        let w = degraded_warning("member p failed: \"quoted\"\nnext");
        assert_eq!(w.to_str().unwrap(), "199 - \"member p failed: quotednext\"");
    }

    #[test]
    fn index_prefix_is_every_segment_between_index_and_name() {
        assert_eq!(index_prefix("/r/index/1/a").as_deref(), Some("1"));
        assert_eq!(index_prefix("/r/index/3/a/abc").as_deref(), Some("3/a"));
        assert_eq!(index_prefix("/r/index/se/rd/serde").as_deref(), Some("se/rd"));
        assert_eq!(index_prefix("/r/index/a"), None);
        assert_eq!(index_prefix("/r/other/1/a"), None);
    }

    #[test]
    fn index_params_need_repo_and_name() {
        let params: HashMap<String, String> = [("repo", "r"), ("name", "a")]
            .into_iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        assert_eq!(index_params(&params).unwrap(), ("r", "a"));
        let mut missing = params.clone();
        missing.remove("name");
        assert!(index_params(&missing).is_err());
    }

    #[test]
    fn if_none_match_accepts_lists_and_star() {
        let mut h = HeaderMap::new();
        h.insert(
            header::IF_NONE_MATCH,
            HeaderValue::from_static("\"x\", \"y\""),
        );
        assert!(if_none_match_hits(&h, "\"y\""));
        assert!(!if_none_match_hits(&h, "\"z\""));
        h.insert(header::IF_NONE_MATCH, HeaderValue::from_static("*"));
        assert!(if_none_match_hits(&h, "\"z\""));
    }

    #[test]
    fn config_body_names_the_requested_repo() {
        let body = config_body("http://h", UrlRepo("cargo-all"), true);
        assert_eq!(body["dl"], "http://h/cargo-all/api/v1/crates");
        assert_eq!(body["api"], "http://h/cargo-all");
        assert_eq!(body["auth-required"], true);
        assert!(config_body("http://h", UrlRepo("x"), false)
            .get("auth-required")
            .is_none());
    }
}
