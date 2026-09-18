use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde_json::json;

use crate::domain::DomainError;

pub type AppResult<T> = Result<T, AppError>;

/// The 409 body of a concurrent double publish: served by the unique-violation
/// sniff below until every format publishes through a store, and by
/// [`StoreError::Conflict`] after that, so the two cannot drift apart.
const CONFLICT_BODY: &str = "resource already exists (conflict)";

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

    /// Transient backend failure (e.g. the DB is unavailable during an authz
    /// check): retryable 503, consistent with the auth middleware's treatment
    /// of DB errors — never a 4xx that would misreport the caller's rights.
    #[error("{0}")]
    ServiceUnavailable(String),

    #[error("{0}")]
    BadGateway(String),

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
            AppError::ServiceUnavailable(msg) => (StatusCode::SERVICE_UNAVAILABLE, msg.clone()),
            AppError::BadGateway(msg) => (StatusCode::BAD_GATEWAY, msg.clone()),
            AppError::Internal(msg) => (StatusCode::INTERNAL_SERVER_ERROR, msg.clone()),
            AppError::Database(err) => {
                // A UNIQUE-constraint violation is a client-visible conflict, not
                // an internal error. It happens e.g. on two concurrent publishes
                // of the same package@version: both pass the pre-insert existence
                // check, then the second INSERT violates UNIQUE(package_id,version).
                // Map it to 409 instead of 500. (Full atomicity via DB
                // transactions around publish/promote remains a follow-up — it
                // needs threading a &mut Transaction through the DAL.)
                if let sqlx::Error::Database(db_err) = err {
                    if db_err.is_unique_violation() {
                        return (
                            StatusCode::CONFLICT,
                            Json(json!({ "error": CONFLICT_BODY })),
                        )
                            .into_response();
                    }
                }
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

/// What any store can fail with, free of the driver and of the dialect: an
/// adapter maps its own failures onto these four, so the layers above never
/// see the persistence technology. Lives beside [`AppError`] until the ports
/// module exists.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("not found")]
    NotFound,

    #[error("resource already exists (conflict)")]
    Conflict,

    #[error("the store is unavailable, try again")]
    Unavailable,

    #[error(transparent)]
    Other(Box<dyn std::error::Error + Send + Sync>),
}

impl From<DomainError> for AppError {
    fn from(err: DomainError) -> Self {
        let message = err.to_string();
        match err {
            DomainError::NotFound(_) => AppError::NotFound(message),
            DomainError::Conflict(_) => AppError::Conflict(message),
            DomainError::Forbidden(_) => AppError::Forbidden(message),
            DomainError::InvalidName(_) => AppError::BadRequest(message),
            DomainError::CorruptColumn { .. } => AppError::Internal(message),
        }
    }
}

impl From<StoreError> for AppError {
    fn from(err: StoreError) -> Self {
        match &err {
            StoreError::NotFound => AppError::NotFound(err.to_string()),
            StoreError::Conflict => AppError::Conflict(CONFLICT_BODY.to_string()),
            StoreError::Unavailable => AppError::ServiceUnavailable(err.to_string()),
            StoreError::Other(source) => {
                tracing::error!("Store error: {source}");
                AppError::Internal("internal server error".to_string())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{Action, Resource};

    async fn served(err: AppError) -> (StatusCode, String) {
        let response = err.into_response();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), 64 * 1024)
            .await
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        (status, body["error"].as_str().unwrap().to_string())
    }

    fn resource() -> Resource {
        Resource {
            kind: "repository",
            id: "npm-hosted".to_string(),
        }
    }

    /// The message equality with `Repository::kind`'s own `corrupt_column` is
    /// asserted next to that method, which may name the row type; this side
    /// pins the status and the body it reaches the client as.
    #[tokio::test]
    async fn corrupt_column_is_a_500_carrying_the_column_it_could_not_read() {
        let ported = AppError::from(DomainError::CorruptColumn {
            repo: "r".to_string(),
            column: "repo_type",
            value: "bogus".to_string(),
        });
        assert_eq!(
            served(ported).await,
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "repository 'r' has a corrupt repo_type column: 'bogus'".to_string()
            )
        );
    }

    #[tokio::test]
    async fn invalid_name_serves_what_validate_package_name_serves_today() {
        let legacy = crate::registry::validate_package_name("npm", "Bad Name").unwrap_err();
        let message = legacy.to_string();
        assert_eq!(message, "invalid npm package name: 'Bad Name'");

        let (legacy_status, legacy_body) = served(legacy).await;
        let (status, body) = served(DomainError::InvalidName(message).into()).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!((status, body), (legacy_status, legacy_body));
    }

    #[tokio::test]
    async fn domain_errors_keep_their_statuses() {
        let cases = [
            (
                AppError::from(DomainError::NotFound(resource())),
                StatusCode::NOT_FOUND,
                "repository 'npm-hosted' not found",
            ),
            (
                AppError::from(DomainError::Conflict(resource())),
                StatusCode::CONFLICT,
                "repository 'npm-hosted' already exists",
            ),
            (
                AppError::from(DomainError::Forbidden(Action {
                    verb: "publish to",
                    on: resource(),
                })),
                StatusCode::FORBIDDEN,
                "not allowed to publish to repository 'npm-hosted'",
            ),
        ];
        for (err, want_status, want_body) in cases {
            assert_eq!(served(err).await, (want_status, want_body.to_string()));
        }
    }

    #[tokio::test]
    async fn store_conflict_serves_the_body_the_unique_violation_sniff_serves() {
        assert_eq!(
            served(StoreError::Conflict.into()).await,
            (StatusCode::CONFLICT, CONFLICT_BODY.to_string())
        );
        assert_eq!(StoreError::Conflict.to_string(), CONFLICT_BODY);
        assert_eq!(
            served(StoreError::NotFound.into()).await,
            (StatusCode::NOT_FOUND, "not found".to_string())
        );
        assert_eq!(
            served(StoreError::Unavailable.into()).await,
            (
                StatusCode::SERVICE_UNAVAILABLE,
                "the store is unavailable, try again".to_string()
            )
        );
    }

    #[tokio::test]
    async fn store_other_is_a_500_that_keeps_its_source_out_of_the_body() {
        let source = std::io::Error::other("/srv/data/db/opencargo.db is locked");
        let (status, body) = served(StoreError::Other(Box::new(source)).into()).await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(body, "internal server error");
    }
}
