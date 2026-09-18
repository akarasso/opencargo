use axum::{routing::get, Router};

use super::http::{deposit, read};
use crate::server::AppState;

pub fn routes() -> Router<AppState> {
    Router::new().route("/maven/{repo}/{*path}", get(read).put(deposit))
}
