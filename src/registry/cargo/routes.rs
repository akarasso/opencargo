use axum::{
    routing::{delete, get, put},
    Router,
};

use super::{config_json, download_crate, get_index_entry, publish_crate, unyank, yank};
use crate::server::AppState;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/{repo}/index/config.json", get(config_json))
        .route("/{repo}/index/1/{name}", get(get_index_entry))
        .route("/{repo}/index/2/{name}", get(get_index_entry))
        .route("/{repo}/index/3/{first}/{name}", get(get_index_entry))
        .route("/{repo}/index/{first_two}/{next_two}/{name}", get(get_index_entry))
        .route("/{repo}/api/v1/crates/new", put(publish_crate))
        .route("/{repo}/api/v1/crates/{name}/{version}/download", get(download_crate))
        .route("/{repo}/api/v1/crates/{name}/{version}/yank", delete(yank))
        .route("/{repo}/api/v1/crates/{name}/{version}/unyank", put(unyank))
}
