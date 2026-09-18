pub mod dist_tags;
pub mod leaves;
pub mod packument;
pub mod publish;
pub mod read;
pub mod routes;
pub mod search;
pub mod upstream;

use std::collections::HashMap;

use crate::auth::middleware::AuthUser;
use crate::domain::Repository;
use crate::error::{AppError, AppResult};
use crate::registry::resolve::{Cx, UrlRepo};
use crate::server::AppState;

fn param<'a>(params: &'a HashMap<String, String>, key: &str) -> AppResult<&'a str> {
    params
        .get(key)
        .map(String::as_str)
        .ok_or_else(|| AppError::BadRequest(format!("missing {key}")))
}

fn cx<'a>(state: &'a AppState, auth: Option<&'a AuthUser>, repo: &'a Repository) -> Cx<'a> {
    Cx {
        state,
        auth,
        url: UrlRepo(&repo.name),
    }
}
