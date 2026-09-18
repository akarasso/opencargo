use axum::{
    routing::{delete, get, put},
    Router,
};

use super::publish::{push, relist, unlist};
use super::read::{
    flat_file, flat_index, registration_index, registration_leaf, registration_page,
    service_index,
};
use crate::server::AppState;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/{repo}/v3/index.json", get(service_index))
        .route("/{repo}/v3/package", put(push))
        .route("/{repo}/v3/package/", put(push))
        .route("/{repo}/api/v2/package", put(push))
        .route("/{repo}/api/v2/package/", put(push))
        .route(
            "/{repo}/v3/package/{id}/{version}",
            delete(unlist).post(relist),
        )
        .route("/{repo}/v3/flatcontainer/{id}/index.json", get(flat_index))
        .route(
            "/{repo}/v3/flatcontainer/{id}/{version}/{file}",
            get(flat_file),
        )
        .route(
            "/{repo}/v3/registration/{id}/index.json",
            get(registration_index),
        )
        .route("/{repo}/v3/registration/{id}/{leaf}", get(registration_leaf))
        .route(
            "/{repo}/v3/registration/{id}/page/{lower}/{upper}",
            get(registration_page),
        )
}
