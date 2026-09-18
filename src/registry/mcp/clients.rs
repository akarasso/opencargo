//! The files a client writes, generated from the servers a repository
//! approves and allows: one renderer per client over one row set, so every
//! file a fleet receives agrees with the catalog.

use std::collections::HashMap;

use axum::{
    extract::{Path, Query, State},
    Extension, Json,
};
use serde_json::{json, Map, Value};

use super::catalog::{open, scope};
use super::ingest::detail_of;
use super::schema::{Package, RemoteTransport, ServerDetail};
use crate::auth::middleware::AuthUser;
use crate::domain::governance::admits;
use crate::domain::{Drift, Format, Visibility};
use crate::error::{AppError, AppResult};
use crate::ports::mcp::PageQuery;
use crate::server::AppState;

/// Where rewritten package commands resolve: an opencargo repository per
/// package ecosystem, with whether a client can read it without a token.
#[derive(Debug, Clone, Default)]
pub struct ClientBase {
    pub base_url: String,
    pub host: String,
    pub repo: String,
    pub npm: Option<(String, bool)>,
    pub pypi: Option<(String, bool)>,
}

/// One approved server, as every renderer sees it.
#[derive(Debug, Clone)]
pub struct Approved {
    pub key: String,
    pub detail: ServerDetail,
}

/// A stdio entry: the argv and its environment, or why it cannot be one.
#[derive(Debug, Clone, PartialEq)]
pub struct Command {
    pub argv: Vec<String>,
    pub env: Map<String, Value>,
}

pub trait ClientRenderer: Send + Sync {
    fn render(&self, rows: &[Approved], base: &ClientBase) -> Value;
}

fn text(v: &Value, key: &str) -> Option<String> {
    v.get(key).and_then(Value::as_str).map(str::to_string)
}

/// A secret is always `${NAME}`, never a value; so is a required input
/// with nothing to fill it.
fn input_value(v: &Value) -> Option<String> {
    let name = text(v, "name").unwrap_or_default();
    if v.get("isSecret").and_then(Value::as_bool).unwrap_or(false) {
        return Some(format!("${{{name}}}"));
    }
    text(v, "value")
        .or_else(|| text(v, "default"))
        .or_else(|| v.get("isRequired").and_then(Value::as_bool).unwrap_or(false).then(|| format!("${{{name}}}")))
}

fn arguments(list: &[Value]) -> Vec<String> {
    let mut out = Vec::new();
    for a in list {
        let value = text(a, "value").or_else(|| text(a, "default")).or_else(|| text(a, "valueHint"));
        match text(a, "type").as_deref() {
            Some("named") => {
                out.extend(text(a, "name"));
                out.extend(value);
            }
            _ => out.extend(value),
        }
    }
    out
}

fn env(list: &[Value]) -> Map<String, Value> {
    list.iter()
        .filter_map(|e| Some((text(e, "name")?, Value::String(input_value(e)?))))
        .collect()
}

/// A stdio package as a command, rewritten onto opencargo's own
/// repository of that ecosystem when one is given. A private one would
/// 401 at install, so the entry is skipped with the credential it needs.
pub fn command(p: &Package, base: &ClientBase) -> Result<Command, String> {
    let versioned = |sep: &str| match &p.version {
        Some(v) => format!("{}{sep}{v}", p.identifier),
        None => p.identifier.clone(),
    };
    let mut argv = arguments(&p.runtime_arguments);
    let program = match p.registry_type.to_ascii_lowercase().as_str() {
        "npm" => {
            let mut head = vec!["npx".to_string(), "-y".to_string()];
            if let Some((repo, public)) = &base.npm {
                if !public {
                    return Err("private npm repository".into());
                }
                head.extend(["--registry".to_string(), format!("{}/{repo}", base.base_url)]);
            }
            head.append(&mut argv);
            head.push(versioned("@"));
            head
        }
        "pypi" => {
            let mut head = vec!["uvx".to_string()];
            if let Some((repo, public)) = &base.pypi {
                if !public {
                    return Err("private pypi repository".into());
                }
                head.extend(["--index-url".to_string(), format!("{}/{repo}/simple/", base.base_url)]);
            }
            head.append(&mut argv);
            head.push(versioned("=="));
            head
        }
        "oci" => {
            let mut head = vec!["docker".to_string(), "run".to_string(), "-i".to_string(), "--rm".to_string()];
            head.append(&mut argv);
            head.push(versioned(":"));
            head
        }
        other => return Err(format!("{other} packages are not rendered")),
    };
    let mut argv = program;
    argv.extend(arguments(&p.package_arguments));
    Ok(Command {
        argv,
        env: env(&p.environment_variables),
    })
}

fn headers(r: &RemoteTransport) -> Map<String, Value> {
    env(&r.headers)
}

fn remote_type(r: &RemoteTransport) -> &'static str {
    if r.kind.eq_ignore_ascii_case("sse") {
        "sse"
    } else {
        "http"
    }
}

/// The first stdio package, else the first remote.
enum Entry {
    Stdio(Command),
    Remote(RemoteTransport),
    Skipped(String),
}

fn entry(row: &Approved, base: &ClientBase) -> Entry {
    let stdio = row.detail.packages.iter().find(|p| p.transport.kind.eq_ignore_ascii_case("stdio"));
    match (stdio, row.detail.remotes.first()) {
        (Some(p), _) => match command(p, base) {
            Ok(c) => Entry::Stdio(c),
            Err(why) => match row.detail.remotes.first() {
                Some(r) => Entry::Remote(r.clone()),
                None => Entry::Skipped(why),
            },
        },
        (None, Some(r)) => Entry::Remote(r.clone()),
        (None, None) => Entry::Skipped("no stdio package and no remote".into()),
    }
}

fn npmrc(base: &ClientBase) -> Option<String> {
    let (repo, public) = base.npm.as_ref()?;
    let host = base.base_url.trim_start_matches("https://").trim_start_matches("http://");
    (!public).then(|| format!("//{host}/{repo}/:_authToken=${{OPENCARGO_TOKEN}}"))
}

fn skipped(key: &str, why: &str) -> Value {
    json!({"name": key, "reason": why})
}

/// `.mcp.json` and `managed-mcp.json`: one shape, secrets as `${VAR}`.
struct McpJson;
/// VS Code's `.vscode/mcp.json`.
struct VsCode;
/// The managed-settings fragment: remote servers provided, every approved
/// command and URL allowlisted, the lock on, the marketplace declared.
struct ManagedSettings;

impl ClientRenderer for McpJson {
    fn render(&self, rows: &[Approved], base: &ClientBase) -> Value {
        let mut servers = Map::new();
        let mut skip = Vec::new();
        for row in rows {
            let value = match entry(row, base) {
                Entry::Stdio(c) => json!({"type": "stdio", "command": c.argv[0], "args": c.argv[1..], "env": c.env}),
                Entry::Remote(r) => json!({"type": remote_type(&r), "url": r.url, "headers": headers(&r)}),
                Entry::Skipped(why) => {
                    skip.push(skipped(&row.key, &why));
                    continue;
                }
            };
            servers.insert(row.key.clone(), value);
        }
        let mut out = json!({"mcpServers": servers});
        if !skip.is_empty() {
            out["skipped"] = Value::Array(skip);
        }
        if let Some(npmrc) = npmrc(base) {
            out["npmrc"] = json!(npmrc);
        }
        out
    }
}

impl ClientRenderer for VsCode {
    fn render(&self, rows: &[Approved], base: &ClientBase) -> Value {
        let mut servers = Map::new();
        for row in rows {
            let value = match entry(row, base) {
                Entry::Stdio(c) => json!({"type": "stdio", "command": c.argv[0], "args": c.argv[1..], "env": c.env}),
                Entry::Remote(r) => json!({"type": remote_type(&r), "url": r.url, "headers": headers(&r)}),
                Entry::Skipped(_) => continue,
            };
            servers.insert(row.key.clone(), value);
        }
        json!({"servers": servers})
    }
}

impl ClientRenderer for ManagedSettings {
    fn render(&self, rows: &[Approved], base: &ClientBase) -> Value {
        let mut managed = Map::new();
        let mut allowed = Vec::new();
        let mut skip = Vec::new();
        for row in rows {
            match entry(row, base) {
                Entry::Stdio(c) => allowed.push(json!({"serverCommand": c.argv})),
                Entry::Remote(r) => {
                    allowed.push(json!({"serverUrl": r.url}));
                    let headers = headers(&r);
                    let literal = !r.url.contains("${") && !headers.values().any(|v| v.as_str().is_some_and(|s| s.contains("${")));
                    if literal {
                        managed.insert(row.key.clone(), json!({"type": remote_type(&r), "url": r.url, "headers": headers}));
                    } else {
                        skip.push(skipped(&row.key, "a managed entry cannot expand a variable"));
                    }
                }
                Entry::Skipped(why) => skip.push(skipped(&row.key, &why)),
            }
        }
        let marketplace = json!({"source": "hostPattern", "hostPattern": format!("^{}$", base.host.replace('.', "\\."))});
        let mut out = json!({
            "requires": "Claude Code v2.1.273 or later",
            "managedMcpServers": managed,
            "allowedMcpServers": allowed,
            "allowManagedMcpServersOnly": true,
            "enableAllProjectMcpServers": false,
            "extraKnownMarketplaces": {format!("opencargo-{}", base.repo): {"source": {"source": "url",
                "url": format!("{}/{}/.claude-plugin/marketplace.json", base.base_url, base.repo)}}},
            "strictKnownMarketplaces": [marketplace],
        });
        if !skip.is_empty() {
            out["skipped"] = Value::Array(skip);
        }
        out
    }
}

pub fn renderer(client: &str) -> Option<&'static dyn ClientRenderer> {
    match client {
        "claude-code" | "claude-code-managed-file" | "cursor" => Some(&McpJson),
        "claude-code-managed" => Some(&ManagedSettings),
        "vscode" => Some(&VsCode),
        _ => None,
    }
}

/// The server's short name as a config key; the full name when two
/// approved servers share it.
fn keys(details: Vec<ServerDetail>) -> Vec<Approved> {
    let leaf = |d: &ServerDetail| d.name.rsplit('/').next().unwrap_or(&d.name).to_string();
    let mut counts: HashMap<String, usize> = HashMap::new();
    for d in &details {
        *counts.entry(leaf(d)).or_default() += 1;
    }
    details
        .into_iter()
        .map(|d| {
            let short = leaf(&d);
            let key = if counts[&short] > 1 { d.name.replace('/', "-") } else { short };
            Approved { key, detail: d }
        })
        .collect()
}

async fn package_repo(state: &AppState, name: Option<&String>, format: Format) -> AppResult<Option<(String, bool)>> {
    let Some(name) = name else {
        return Ok(None);
    };
    let repo = crate::registry::load_repo(state.repos.as_ref(), name).await?;
    crate::registry::ensure_format(&repo, format)?;
    Ok(Some((repo.name.clone(), repo.visibility == Visibility::Public)))
}

/// GET /{repo}/clients/{client}/config.json[?npm=<repo>&pypi=<repo>]
pub async fn config(
    State(state): State<AppState>,
    Path((repo_name, client)): Path<(String, String)>,
    auth: Option<Extension<AuthUser>>,
    Query(params): Query<HashMap<String, String>>,
) -> AppResult<Json<Value>> {
    let auth = auth.as_ref().map(|e| &e.0);
    let renderer = renderer(&client).ok_or_else(|| AppError::NotFound(format!("unknown client: {client}")))?;
    let repo = open(&state, &repo_name, auth).await?;
    let (members, gates) = scope(&state, &repo, auth).await?;
    let mut seen = std::collections::HashSet::new();
    let mut details = Vec::new();
    for member in &members {
        let mut after = None;
        loop {
            let rows = state
                .mcp
                .page(&PageQuery {
                    member: member.id,
                    addressed: repo.id,
                    after: after.clone(),
                    limit: 500,
                    latest_only: true,
                    filters: gates.filters(member.id),
                    ..PageQuery::default()
                })
                .await?;
            let exhausted = rows.len() < 500;
            for row in rows {
                after = Some((row.name.clone(), row.version.clone()));
                let approved = row.surface_endpoints > 0
                    && row.approved_endpoints == row.surface_endpoints
                    && row.worst_drift == Drift::None;
                let served = gates.decide(member.id, &row, false).served();
                if approved && served && admits(&gates.addressed.rules, &row.name) && seen.insert(row.name.clone()) {
                    details.push(detail_of(&row.envelope_json)?);
                }
            }
            if exhausted {
                break;
            }
        }
    }
    let host = state
        .base_url
        .trim_start_matches("https://")
        .trim_start_matches("http://")
        .split(['/', ':'])
        .next()
        .unwrap_or_default()
        .to_string();
    let base = ClientBase {
        base_url: state.base_url.clone(),
        host,
        repo: repo.name.clone(),
        npm: package_repo(&state, params.get("npm"), Format::Npm).await?,
        pypi: package_repo(&state, params.get("pypi"), Format::Pypi).await?,
    };
    Ok(Json(renderer.render(&keys(details), &base)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::mcp::schema::parse;

    fn server(v: Value) -> ServerDetail {
        let mut base = json!({"description": "d", "version": "1.0.0"});
        base.as_object_mut().unwrap().extend(v.as_object().unwrap().clone());
        parse(&base).unwrap().0
    }

    fn base(npm_public: bool) -> ClientBase {
        ClientBase {
            base_url: "https://opencargo.example.com".into(),
            host: "opencargo.example.com".into(),
            repo: "mcp-platform".into(),
            npm: Some(("npm-platform".into(), npm_public)),
            pypi: None,
        }
    }

    fn rows() -> Vec<Approved> {
        keys(vec![
            server(json!({"name": "io.github.acme/acme-search", "packages": [{"registryType": "npm",
                "identifier": "@acme/search-mcp", "version": "1.4.2", "transport": {"type": "stdio"},
                "environmentVariables": [{"name": "ACME_TOKEN", "isSecret": true, "value": "hunter2"}]}]})),
            server(json!({"name": "com.acme/billing", "remotes": [{"type": "streamable-http",
                "url": "https://mcp.billing.internal/mcp", "headers": [{"name": "X-Acme-Tenant", "value": "platform"}]}]})),
            server(json!({"name": "com.acme/vars", "remotes": [{"type": "streamable-http",
                "url": "https://mcp.vars.internal/mcp", "headers": [{"name": "Authorization", "isSecret": true}]}]})),
        ])
    }

    #[test]
    fn claude_code_config_rewrites_npx_onto_the_local_npm_repo_and_never_emits_a_secret() {
        let out = McpJson.render(&rows(), &base(true));
        let search = &out["mcpServers"]["acme-search"];
        assert_eq!(search["command"], "npx");
        assert_eq!(
            search["args"],
            json!(["-y", "--registry", "https://opencargo.example.com/npm-platform", "@acme/search-mcp@1.4.2"])
        );
        assert_eq!(search["env"]["ACME_TOKEN"], "${ACME_TOKEN}");
        assert!(!out.to_string().contains("hunter2"));
        assert_eq!(out["mcpServers"]["billing"], json!({"type": "http", "url": "https://mcp.billing.internal/mcp",
            "headers": {"X-Acme-Tenant": "platform"}}));
    }

    #[test]
    fn private_npm_repo_yields_a_credential_fragment_not_a_config_that_401s() {
        let out = McpJson.render(&rows(), &base(false));
        assert!(out["mcpServers"].get("acme-search").is_none());
        assert_eq!(out["skipped"][0]["reason"], "private npm repository");
        assert_eq!(out["npmrc"], "//opencargo.example.com/npm-platform/:_authToken=${OPENCARGO_TOKEN}");
    }

    #[test]
    fn the_managed_fragment_allowlists_commands_and_urls_and_provides_literal_remotes_only() {
        let out = ManagedSettings.render(&rows(), &base(true));
        assert!(out["managedMcpServers"].is_object(), "an object keyed by name, never an array");
        assert!(out["managedMcpServers"].get("billing").is_some());
        assert!(out["managedMcpServers"].get("vars").is_none(), "a ${{VAR}} cannot be expanded there");
        assert!(out["managedMcpServers"].get("acme-search").is_none(), "stdio is never a managed entry");
        let allowed = out["allowedMcpServers"].as_array().unwrap();
        assert!(allowed.iter().all(|a| a.get("serverName").is_none()));
        assert!(allowed.iter().any(|a| a["serverUrl"] == "https://mcp.vars.internal/mcp"), "a var-bearing entry is still allowlisted");
        assert!(allowed.iter().any(|a| a["serverCommand"][0] == "npx"));
        assert_eq!(out["allowManagedMcpServersOnly"], true);
        assert!(out["extraKnownMarketplaces"]["opencargo-mcp-platform"].is_object());
        assert_eq!(out["strictKnownMarketplaces"][0]["hostPattern"], "^opencargo\\.example\\.com$");
    }

    #[test]
    fn cursor_and_vscode_renderers_agree_on_the_same_row_set() {
        let cursor = McpJson.render(&rows(), &base(true));
        let vscode = VsCode.render(&rows(), &base(true));
        let names = |v: &Value| v.as_object().unwrap().keys().cloned().collect::<Vec<_>>();
        assert_eq!(names(&cursor["mcpServers"]), names(&vscode["servers"]));
    }
}
