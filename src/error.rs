use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde_json::json;

pub type AppResult<T> = Result<T, AppError>;

#[derive(Debug, thiserror::Error)]
pub enum AppError {
    #[error("{0}")]
    NotFound(String),

    #[error("{0}")]
    BadRequest(String),

    #[error("{0}")]
    Unauthorized(String),

    #[error("{0}")]
    Forbidden(String),

    #[error("{0}")]
    Conflict(String),

    #[error("{0}")]
    TooManyRequests(String),

    #[error("{0}")]
    Internal(String),

    #[error(transparent)]
    Database(#[from] sqlx::Error),

    #[error(transparent)]
    Io(#[from] std::io::Error),

    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let (status, message) = match &self {
            AppError::NotFound(msg) => (StatusCode::NOT_FOUND, msg.clone()),
            AppError::BadRequest(msg) => (StatusCode::BAD_REQUEST, msg.clone()),
            AppError::Unauthorized(msg) => (StatusCode::UNAUTHORIZED, msg.clone()),
            AppError::Forbidden(msg) => (StatusCode::FORBIDDEN, msg.clone()),
            AppError::Conflict(msg) => (StatusCode::CONFLICT, msg.clone()),
            AppError::TooManyRequests(msg) => (StatusCode::TOO_MANY_REQUESTS, msg.clone()),
            AppError::Internal(msg) => (StatusCode::INTERNAL_SERVER_ERROR, msg.clone()),
            AppError::Database(err) => {
                tracing::error!("Database error: {}", err);
                (StatusCode::INTERNAL_SERVER_ERROR, "internal server error".to_string())
            }
            AppError::Io(err) => {
                tracing::error!("IO error: {}", err);
                (StatusCode::INTERNAL_SERVER_ERROR, "internal server error".to_string())
            }
            AppError::Json(err) => {
                tracing::debug!("JSON parse error: {}", err);
                (StatusCode::BAD_REQUEST, "invalid JSON body".to_string())
            }
        };

        let body = Json(json!({ "error": message }));

        (status, body).into_response()
    }
}
