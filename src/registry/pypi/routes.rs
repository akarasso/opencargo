use axum::{
    routing::{get, post},
    Router,
};

use super::manage::{delete_project, delete_release, unyank, yank};
use super::read::{file, index, project, project_no_slash};
use super::upload::upload;
use crate::server::AppState;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/{repo}/simple/", get(index))
        .route("/{repo}/simple/{project}", get(project_no_slash))
        .route("/{repo}/simple/{project}/", get(project))
        .route("/{repo}/files/{project}/{filename}", get(file))
        .route("/{repo}/legacy/", post(upload))
        .route("/{repo}/legacy", post(upload))
        .route("/{repo}/pypi/{project}/{version}/yank", post(yank).delete(unyank))
        .route("/{repo}/pypi/{project}/{version}", axum::routing::delete(delete_release))
        .route("/{repo}/pypi/{project}", axum::routing::delete(delete_project))
}
