pub mod dist_tags;
pub mod leaves;
pub mod packument;
pub mod publish;
pub mod read;
pub mod render;
pub mod routes;
pub mod search;
pub mod upstream;

use std::collections::HashMap;

use crate::error::{AppError, AppResult};

fn param<'a>(params: &'a HashMap<String, String>, key: &str) -> AppResult<&'a str> {
    params
        .get(key)
        .map(String::as_str)
        .ok_or_else(|| AppError::BadRequest(format!("missing {key}")))
}

