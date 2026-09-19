use axum::{routing::get, Router};

use super::http::{delete, put, read};
use crate::server::AppState;

pub fn routes() -> Router<AppState> {
    Router::new().route("/raw/{repo}/{*path}", get(read).put(put).delete(delete))
}
