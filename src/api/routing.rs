//! The admin surface of the routing rules, and the dry run that makes one
//! reviewable before it is activated.

use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
    Json,
};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::api::{actor, require_admin, require_auth};
use crate::app::routing::{Explanation, RoutingRules, RuleDraft};
use crate::domain::Format;
use crate::error::{AppError, AppResult};
use crate::ports::routing::StoredRule;
use crate::server::AppState;
use crate::wire::wire_ts;

#[derive(Deserialize)]
pub struct RuleRequest {
    pub name: Option<String>,
    pub format: String,
    #[serde(default)]
    pub patterns: Vec<String>,
    #[serde(default)]
    pub except: Vec<String>,
    pub effect: String,
    #[serde(default)]
    pub targets: Vec<String>,
    #[serde(default)]
    pub confirm_catch_all: bool,
    /// A rule is total over its format. A request that carries a scope is
    /// refused rather than served with the field ignored (D1): a control
    /// whose narrowing was silently dropped is worse than no control.
    #[serde(default)]
    pub scope: Option<Value>,
    #[serde(default)]
    pub repositories: Option<Value>,
}

impl RuleRequest {
    fn draft<'a>(&'a self, name: &'a str) -> AppResult<RuleDraft<'a>> {
        if self.scope.is_some() || self.repositories.is_some() {
            return Err(AppError::BadRequest(
                "a routing rule applies to every repository of its format; it takes no scope"
                    .to_string(),
            ));
        }
        Ok(RuleDraft {
            name,
            format: self.format.parse::<Format>()?,
            patterns: &self.patterns,
            except: &self.except,
            effect: &self.effect,
            targets: &self.targets,
            confirm_catch_all: self.confirm_catch_all,
        })
    }
}

#[derive(Deserialize)]
pub struct ExplainRequest {
    pub repository: String,
    pub name: String,
    #[serde(default)]
    pub candidate: Option<RuleRequest>,
}

fn rule_json(rule: &StoredRule) -> Value {
    let (effect, targets) = match &rule.effect {
        crate::domain::Effect::Members(incarnations) => ("allow_members", incarnations.clone()),
        other => (other.as_str(), Vec::new()),
    };
    json!({
        "name": rule.name,
        "format": rule.format.as_str(),
        "patterns": rule.patterns,
        "except": rule.except,
        "effect": effect,
        "targets": targets,
        "created_at": wire_ts(rule.created_at),
        "updated_at": wire_ts(rule.updated_at),
    })
}

fn explanation_json(explained: &Explanation) -> Value {
    json!({
        "match_key": explained.match_key,
        "ident_key": explained.ident_key,
        "snapshot_version": explained.version,
        "members": explained.members.iter().map(|m| json!({
            "name": m.name,
            "kind": m.kind.as_str(),
            "admitted": m.admitted,
            "refused_by": m.refused_by,
            "stale": m.stale,
        })).collect::<Vec<_>>(),
    })
}

fn rules(state: &AppState) -> RoutingRules {
    RoutingRules::new(
        state.routing_rules.clone(),
        state.repos.clone(),
        state.routing.clone(),
        state.audit.clone(),
        state.events.clone(),
    )
}

pub async fn list_rules(
    State(state): State<AppState>,
    request: axum::http::Request<axum::body::Body>,
) -> AppResult<impl IntoResponse> {
    require_admin(&require_auth(&request)?)?;
    let all = rules(&state).all().await?;
    Ok(Json(json!({
        "rules": all.iter().map(rule_json).collect::<Vec<_>>(),
        "snapshot_version": state.routing.version(),
    })))
}

pub async fn create_rule(
    State(state): State<AppState>,
    request: axum::http::Request<axum::body::Body>,
) -> AppResult<impl IntoResponse> {
    let caller = require_auth(&request)?;
    require_admin(&caller)?;
    let body: RuleRequest = read_json(request).await?;
    let name = body
        .name
        .clone()
        .ok_or_else(|| AppError::BadRequest("a routing rule needs a name".to_string()))?;
    let written = rules(&state)
        .create(&body.draft(&name)?, &actor(&caller), chrono::Utc::now())
        .await?;
    Ok((StatusCode::CREATED, Json(rule_json(&written))))
}

pub async fn get_rule(
    State(state): State<AppState>,
    Path(name): Path<String>,
    request: axum::http::Request<axum::body::Body>,
) -> AppResult<impl IntoResponse> {
    require_admin(&require_auth(&request)?)?;
    Ok(Json(rule_json(&rules(&state).by_name(&name).await?)))
}

pub async fn update_rule(
    State(state): State<AppState>,
    Path(name): Path<String>,
    request: axum::http::Request<axum::body::Body>,
) -> AppResult<impl IntoResponse> {
    let caller = require_auth(&request)?;
    require_admin(&caller)?;
    let body: RuleRequest = read_json(request).await?;
    let written = rules(&state)
        .update(&body.draft(&name)?, &actor(&caller), chrono::Utc::now())
        .await?;
    Ok(Json(rule_json(&written)))
}

/// Deleting a rule reopens what it refused — unless another rule still
/// refuses the same names, which the answer says rather than letting the
/// caller believe in a reopening that did not happen (D12, D9bis).
pub async fn delete_rule(
    State(state): State<AppState>,
    Path(name): Path<String>,
    request: axum::http::Request<axum::body::Body>,
) -> AppResult<impl IntoResponse> {
    let caller = require_auth(&request)?;
    require_admin(&caller)?;
    let rules = rules(&state);
    let removed = rules.by_name(&name).await?;
    rules.delete(&name, &actor(&caller), chrono::Utc::now()).await?;
    let still: Vec<String> = rules
        .all()
        .await?
        .into_iter()
        .filter(|r| r.format == removed.format && r.patterns.iter().any(|p| removed.patterns.contains(p)))
        .map(|r| r.name)
        .collect();
    Ok(Json(json!({
        "deleted": name,
        "still_refused_by": still,
    })))
}

pub async fn explain_route(
    State(state): State<AppState>,
    request: axum::http::Request<axum::body::Body>,
) -> AppResult<impl IntoResponse> {
    require_admin(&require_auth(&request)?)?;
    let body: ExplainRequest = read_json(request).await?;
    let candidate = match &body.candidate {
        Some(rule) => Some(rule.draft(rule.name.as_deref().unwrap_or("candidate"))?),
        None => None,
    };
    let explained = rules(&state)
        .explain(&body.repository, &body.name, candidate.as_ref())
        .await?;
    Ok(Json(explanation_json(&explained)))
}

async fn read_json<T: serde::de::DeserializeOwned>(
    request: axum::http::Request<axum::body::Body>,
) -> AppResult<T> {
    let bytes = axum::body::to_bytes(request.into_body(), 1024 * 1024)
        .await
        .map_err(|e| AppError::BadRequest(format!("invalid request body: {e}")))?;
    serde_json::from_slice(&bytes)
        .map_err(|e| AppError::BadRequest(format!("invalid request body: {e}")))
}
