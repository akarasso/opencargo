use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
    Json,
};
use serde::Deserialize;
use serde_json::json;

use crate::api::{actor, require_admin, require_auth};
use crate::app::maven::admin::Decision;
use crate::domain::Format;
use crate::error::{AppError, AppResult};
use crate::ports::maven::UnitKey;
use crate::registry::maven::hosted::scopes_of_unit;
use crate::server::AppState;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DecideRequest {
    pub group_id: String,
    pub artifact_id: String,
    pub version: String,
    #[serde(default)]
    pub build: String,
    pub decision: String,
}

/// POST /api/v1/maven/{repo}/decide -- promote or refuse a pending unit
/// (admin only, audited).
pub async fn decide(
    State(state): State<AppState>,
    Path(repo_name): Path<String>,
    request: axum::http::Request<axum::body::Body>,
) -> AppResult<impl IntoResponse> {
    let caller = require_auth(&request)?;
    require_admin(&caller)?;
    let bytes = axum::body::to_bytes(request.into_body(), 64 * 1024)
        .await
        .map_err(|e| AppError::BadRequest(format!("failed to read body: {e}")))?;
    let body: DecideRequest = serde_json::from_slice(&bytes)?;
    let decision = match body.decision.as_str() {
        "promote" => Decision::Promote,
        "refuse" => Decision::Refuse,
        other => return Err(AppError::BadRequest(format!("unknown decision: {other}"))),
    };
    let repo = crate::registry::load_repo(state.repos.as_ref(), &repo_name).await?;
    crate::registry::ensure_format(&repo, Format::Maven)?;
    crate::registry::ensure_hosted(&repo)?;
    let ga = format!("{}:{}", body.group_id, body.artifact_id);
    let key = UnitKey {
        repository: repo.id,
        ga: &ga,
        version: &body.version,
        build: &body.build,
    };
    let scopes = scopes_of_unit(&ga, &body.version);
    state
        .decide_maven_unit()
        .run(&key, decision, &scopes, &actor(&caller), state.clock.now())
        .await?;
    Ok((StatusCode::OK, Json(json!({"ok": true}))))
}
