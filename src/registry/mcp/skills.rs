//! Agent skills as Claude Code plugins: a zip with `.claude-plugin/` at the
//! top level (or one folder down) and `skills/<name>/SKILL.md`, whose
//! frontmatter is a permission surface and a model-facing text.

use axum::{
    body::Body,
    extract::{Path, State},
    http::{header, StatusCode},
    response::{IntoResponse, Response},
    Extension, Json,
};
use bytes::Bytes;
use serde_json::{json, Value};

use super::catalog::{open, scope};
use super::ingest::new_findings;
use super::rules::McpRules;
use super::scan::{scan, Field, Text};
use super::surface::canonical_sha256;
use crate::app::mcp::skills::{PublishSkill, SkillUpload};
use crate::auth::middleware::AuthUser;
use crate::domain::{Format, FormatRules, RepoKind, Repository};
use crate::error::{AppError, AppResult};
use crate::ports::mcp::SkillRow;
use crate::registry::archive::{zip_check, zip_names, zip_read, ArchiveError, Limits};
use crate::server::AppState;

const MAX_ARCHIVE_BYTES: usize = 5 << 20;
const MAX_MEMBER_BYTES: u64 = 1 << 20;
const SKILL_LIMITS: Limits = Limits {
    max_members: Some(256),
    max_inflated: Some(5 << 20),
};

/// A plugin name is one path-free segment: it is what approvals key on.
pub fn validate_name(name: &str) -> AppResult<()> {
    let ok = !name.is_empty()
        && name.len() <= 64
        && name.bytes().next().is_some_and(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        && name.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'-' | b'_' | b'.'));
    if ok {
        Ok(())
    } else {
        Err(AppError::BadRequest(format!("invalid skill name: '{name}'")))
    }
}

/// `SKILL.md`'s YAML frontmatter: the description and the allowed tools,
/// read line by line; the rest is the body.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Frontmatter {
    pub description: Option<String>,
    pub allowed_tools: Vec<String>,
    pub body: String,
}

fn unquote(v: &str) -> String {
    v.trim().trim_matches(|c| c == '"' || c == '\'').to_string()
}

pub fn frontmatter(md: &str) -> Frontmatter {
    let mut out = Frontmatter::default();
    let text = md.strip_prefix('\u{feff}').unwrap_or(md);
    let Some(rest) = text.strip_prefix("---\n").or_else(|| text.strip_prefix("---\r\n")) else {
        out.body = text.to_string();
        return out;
    };
    let Some(end) = rest.find("\n---") else {
        out.body = text.to_string();
        return out;
    };
    let (head, tail) = rest.split_at(end);
    out.body = tail.trim_start_matches("\n---").trim_start_matches(['\r', '\n']).to_string();
    let mut in_tools = false;
    for line in head.lines() {
        if in_tools {
            if let Some(item) = line.trim_start().strip_prefix("- ") {
                out.allowed_tools.push(unquote(item));
                continue;
            }
            in_tools = false;
        }
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        match key.trim() {
            "description" => out.description = Some(unquote(value)),
            "allowed-tools" => {
                let value = value.trim().trim_start_matches('[').trim_end_matches(']');
                if value.is_empty() {
                    in_tools = true;
                } else {
                    out.allowed_tools.extend(value.split(',').map(unquote).filter(|t| !t.is_empty()));
                }
            }
            _ => {}
        }
    }
    out
}

pub fn texts_of_skill<'a>(f: &'a Frontmatter, allowed: &'a str) -> Vec<Text<'a>> {
    let mut out = Vec::new();
    if let Some(d) = &f.description {
        out.push((Field::SkillDescription, None, d.as_str()));
    }
    if !allowed.is_empty() {
        out.push((Field::SkillAllowedTools, None, allowed));
    }
    out.push((Field::SkillBody, None, f.body.as_str()));
    out
}

fn bad(e: ArchiveError) -> AppError {
    AppError::BadRequest(format!("invalid skill archive: {e}"))
}

/// Where `.claude-plugin/` sits: the top level, or one single folder down.
fn plugin_root(names: &[String]) -> AppResult<String> {
    if names.iter().any(|n| n == ".claude-plugin/plugin.json") {
        return Ok(String::new());
    }
    let tops: std::collections::BTreeSet<&str> = names.iter().filter_map(|n| n.split('/').next()).collect();
    if tops.len() == 1 {
        let top = format!("{}/", tops.into_iter().next().unwrap_or_default());
        if names.iter().any(|n| *n == format!("{top}.claude-plugin/plugin.json")) {
            return Ok(top);
        }
    }
    Err(AppError::BadRequest(
        "invalid skill archive: no .claude-plugin/plugin.json at the top level or one folder down".into(),
    ))
}

struct Parsed {
    upload: SkillUpload,
}

fn parse_archive(repository: i64, name: &str, version: &str, bytes: Bytes, by: &str) -> AppResult<Parsed> {
    zip_check(&bytes, SKILL_LIMITS).map_err(bad)?;
    let names = zip_names(&bytes).map_err(bad)?;
    let root = plugin_root(&names)?;
    let plugin: Value = serde_json::from_slice(&zip_read(&bytes, &format!("{root}.claude-plugin/plugin.json"), MAX_MEMBER_BYTES).map_err(bad)?)
        .map_err(|e| AppError::BadRequest(format!("invalid plugin.json: {e}")))?;
    if plugin.get("name").and_then(Value::as_str) != Some(name) {
        return Err(AppError::BadRequest(format!("plugin.json names another plugin than {name}")));
    }
    if plugin.get("version").and_then(Value::as_str).is_some_and(|v| v != version) {
        return Err(AppError::BadRequest(format!("plugin.json carries another version than {version}")));
    }
    let skill_files: Vec<&String> = names
        .iter()
        .filter(|n| {
            n.strip_prefix(&root)
                .and_then(|rest| rest.strip_prefix("skills/"))
                .and_then(|rest| rest.split_once('/'))
                .is_some_and(|(dir, file)| !dir.is_empty() && file == "SKILL.md")
        })
        .collect();
    if skill_files.is_empty() {
        return Err(AppError::BadRequest("invalid skill archive: no skills/<name>/SKILL.md".into()));
    }
    let mut surface = Vec::new();
    let mut findings = Vec::new();
    let mut descriptions = Vec::new();
    let mut tools_all = Vec::new();
    for file in skill_files {
        let md = String::from_utf8(zip_read(&bytes, file, MAX_MEMBER_BYTES).map_err(bad)?)
            .map_err(|_| AppError::BadRequest(format!("{file} is not UTF-8")))?;
        let fm = frontmatter(&md);
        let allowed = fm.allowed_tools.join(", ");
        findings.extend(scan(&texts_of_skill(&fm, &allowed), &[]));
        surface.push(json!({"skill": file, "description": fm.description, "allowedTools": fm.allowed_tools}));
        descriptions.extend(fm.description.clone());
        tools_all.extend(fm.allowed_tools.clone());
    }
    let blocking = findings
        .iter()
        .filter(|f| f.native_high() && f.field.is_skill_frontmatter())
        .count() as i64;
    Ok(Parsed {
        upload: SkillUpload {
            repository,
            name: name.to_string(),
            version: version.to_string(),
            bytes,
            description: descriptions.first().cloned().or_else(|| plugin.get("description").and_then(Value::as_str).map(str::to_string)),
            allowed_tools: (!tools_all.is_empty()).then(|| tools_all.join(", ")),
            surface_sha256: canonical_sha256(&Value::Array(surface)),
            findings: new_findings(findings),
            blocking,
            published_by: Some(by.to_string()),
        },
    })
}

fn caller(request: &axum::http::Request<Body>) -> AppResult<AuthUser> {
    request
        .extensions()
        .get::<AuthUser>()
        .cloned()
        .ok_or_else(|| AppError::Unauthorized("authentication required".to_string()))
}

async fn hosted_mcp(state: &AppState, name: &str, auth: &AuthUser) -> AppResult<Repository> {
    let repo = crate::registry::load_repo(state.repos.as_ref(), name).await?;
    crate::registry::ensure_format(&repo, Format::Mcp)?;
    crate::registry::ensure_hosted(&repo)?;
    crate::registry::ensure_can_write(&*state.permissions, &repo, auth).await?;
    Ok(repo)
}

/// PUT /{repo}/skills/{name}/{version}/skill.zip
pub async fn upload(
    State(state): State<AppState>,
    Path((repo_name, name, version)): Path<(String, String, String)>,
    request: axum::http::Request<Body>,
) -> AppResult<(StatusCode, Json<Value>)> {
    let auth = caller(&request)?;
    validate_name(&name)?;
    McpRules.validate_version(&version)?;
    let repo = hosted_mcp(&state, &repo_name, &auth).await?;
    let bytes = axum::body::to_bytes(request.into_body(), MAX_ARCHIVE_BYTES)
        .await
        .map_err(|e| AppError::BadRequest(format!("skill archive over {MAX_ARCHIVE_BYTES} bytes or unreadable: {e}")))?;
    let parsed = {
        let (repo_id, name, version, by) = (repo.id, name.clone(), version.clone(), auth.username.clone());
        tokio::task::spawn_blocking(move || parse_archive(repo_id, &name, &version, bytes, &by))
            .await
            .map_err(|e| AppError::Internal(e.to_string()))??
    };
    let high = parsed.upload.findings.iter().filter(|f| f.high).count();
    let blocking = parsed.upload.blocking;
    let surface = parsed.upload.surface_sha256.clone();
    let id = PublishSkill::new(state.mcp.clone(), state.repos.clone(), state.placer())
        .run(parsed.upload, state.clock.now())
        .await?;
    crate::api::record_audit(&state, &auth, "mcp.skill.publish", Some(&format!("{name}@{version}"))).await;
    Ok((
        StatusCode::CREATED,
        Json(json!({"id": id, "name": name, "version": version, "surfaceSha256": surface,
                    "findings": {"high": high, "blocking": blocking}})),
    ))
}

/// DELETE /{repo}/skills/{name}/{version}/skill.zip
pub async fn delete(
    State(state): State<AppState>,
    Path((repo_name, name, version)): Path<(String, String, String)>,
    request: axum::http::Request<Body>,
) -> AppResult<StatusCode> {
    let auth = caller(&request)?;
    let repo = hosted_mcp(&state, &repo_name, &auth).await?;
    state.mcp.delete_skill(repo.id, &name, &version, state.clock.now()).await?;
    crate::api::record_audit(&state, &auth, "mcp.skill.delete", Some(&format!("{name}@{version}"))).await;
    Ok(StatusCode::NO_CONTENT)
}

/// The skills a repository distributes: each hosted member's latest
/// version per name, gated, and never one whose frontmatter carries a
/// natively high finding, since a marketplace has nowhere to show a flag.
async fn distributed(state: &AppState, repo: &Repository, auth: Option<&AuthUser>) -> AppResult<Vec<SkillRow>> {
    let (members, gates) = scope(state, repo, auth).await?;
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for member in members.iter().filter(|m| m.kind().ok() == Some(RepoKind::Hosted)) {
        for skill in state.mcp.skills(member.id, repo.id).await? {
            if !seen.insert(skill.name.clone()) {
                continue;
            }
            if skill.blocking == 0 && gates.decide_skill(member.id, &skill.name, skill.decision).served() {
                out.push(skill);
            }
        }
    }
    Ok(out)
}

/// GET /{repo}/skills/{name}/{version}/skill.zip
pub async fn download(
    State(state): State<AppState>,
    Path((repo_name, name, version)): Path<(String, String, String)>,
    auth: Option<Extension<AuthUser>>,
) -> AppResult<Response> {
    let auth = auth.as_ref().map(|e| &e.0);
    let repo = open(&state, &repo_name, auth).await?;
    let (members, gates) = scope(&state, &repo, auth).await?;
    for member in &members {
        let Some(skill) = state.mcp.skill(member.id, repo.id, &name, &version).await? else {
            continue;
        };
        if skill.blocking > 0 || !gates.decide_skill(member.id, &skill.name, skill.decision).served() {
            break;
        }
        let bytes = state.storage.get(&skill.key).await?;
        return Ok(([(header::CONTENT_TYPE, "application/zip")], bytes).into_response());
    }
    Err(AppError::NotFound(format!("skill not found: {name}@{version}")))
}

/// GET /{repo}/.claude-plugin/marketplace.json
pub async fn marketplace(
    State(state): State<AppState>,
    Path(repo_name): Path<String>,
    auth: Option<Extension<AuthUser>>,
) -> AppResult<Json<Value>> {
    let auth = auth.as_ref().map(|e| &e.0);
    let repo = open(&state, &repo_name, auth).await?;
    let helper = state.mcp_settings.get(&repo.name).and_then(|c| c.headers_helper.clone());
    let plugins: Vec<Value> = distributed(&state, &repo, auth)
        .await?
        .into_iter()
        .map(|s| {
            let mut source = json!({
                "source": "archive",
                "url": format!("{}/{}/skills/{}/{}/skill.zip", state.base_url, repo.name, s.name, s.version),
                "sha256": s.sha256,
            });
            if let Some(helper) = &helper {
                source["headersHelper"] = json!(helper);
            }
            json!({"name": s.name, "version": s.version, "description": s.description, "source": source})
        })
        .collect();
    Ok(Json(json!({
        "name": format!("opencargo-{}", repo.name),
        "owner": {"name": "opencargo"},
        "plugins": plugins,
    })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frontmatter_reads_description_and_both_tool_list_spellings() {
        let inline = frontmatter("---\nname: deploy\ndescription: \"Deploys the app\"\nallowed-tools: Read, Bash(git:*)\n---\n# Deploy\nbody");
        assert_eq!(inline.description.as_deref(), Some("Deploys the app"));
        assert_eq!(inline.allowed_tools, vec!["Read", "Bash(git:*)"]);
        assert_eq!(inline.body, "# Deploy\nbody");
        let block = frontmatter("---\ndescription: d\nallowed-tools:\n  - Read\n  - Grep\nother: x\n---\nb");
        assert_eq!(block.allowed_tools, vec!["Read", "Grep"]);
        let bare = frontmatter("# no frontmatter");
        assert_eq!((bare.description, bare.body.as_str()), (None, "# no frontmatter"));
    }

    #[test]
    fn a_skill_name_is_one_path_free_segment() {
        for good in ["deploy-runbook", "a", "x_1.2"] {
            assert!(validate_name(good).is_ok(), "{good}");
        }
        for bad in ["", "io.github.acme/x", "../x", "Deploy", "-x", &"x".repeat(65)] {
            assert!(validate_name(bad).is_err(), "{bad}");
        }
    }
}
