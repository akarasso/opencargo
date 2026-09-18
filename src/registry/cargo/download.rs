use axum::{
    extract::{Path, State},
    http::{header, HeaderValue},
    response::Response,
};

use crate::auth::middleware::AuthUser;
use crate::error::{AppError, AppResult};
use crate::registry::resolve::{first_hit, Cx, UrlRepo};
use crate::server::AppState;

use super::leaves::CrateLeaf;

pub async fn download_crate(
    State(state): State<AppState>,
    Path((repo_name, name, version)): Path<(String, String, String)>,
    auth: Option<axum::Extension<AuthUser>>,
) -> AppResult<Response> {
    crate::domain::validate_package_name("cargo", &name)?;
    crate::domain::validate_version(&version)?;
    let repo = crate::registry::load_repo(&state.db, &repo_name).await?;
    let auth = auth.as_ref().map(|e| &e.0);
    crate::registry::ensure_can_read(&state.db, &repo, auth).await?;

    let cx = Cx {
        state: &state,
        auth,
        url: UrlRepo(&repo.name),
    };
    let leaf = CrateLeaf {
        name: name.clone(),
        version: version.clone(),
    };
    let mut payload = first_hit(&cx, &repo, &leaf).await?;
    payload.content_type = Some("application/x-tar".to_string());
    let disposition =
        HeaderValue::from_str(&format!("attachment; filename=\"{name}-{version}.crate\""))
            .map_err(|e| AppError::Internal(format!("invalid content disposition: {e}")))?;
    state
        .proxy
        .stream_response(&payload, vec![(header::CONTENT_DISPOSITION, disposition)])
        .await
}
