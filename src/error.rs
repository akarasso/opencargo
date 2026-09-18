use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde_json::json;

use crate::domain::DomainError;
use crate::storage::StorageError;

pub type AppResult<T> = Result<T, AppError>;

/// The 409 body of a concurrent double publish. Every format now publishes
/// through a store, so the race is the adapter's unique-violation turned into
/// [`StoreError::Conflict`] — there is no second spelling of it left.
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

    /// A commit whose pins were revoked wrote nothing; these physical keys
    /// may already be gone.
    #[error("the placement was superseded, try again")]
    Superseded(Vec<String>),

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

/// The storage port's four refusals, each keeping the status the concrete
/// filesystem backend served before it had a port error of its own —
/// `InvalidPath` above all, which is the traversal guard's 400.
impl From<StorageError> for AppError {
    fn from(err: StorageError) -> Self {
        let message = err.to_string();
        match err {
            StorageError::NotFound => AppError::NotFound(message),
            StorageError::InvalidPath(_) => AppError::BadRequest(message),
            StorageError::Unavailable => AppError::ServiceUnavailable(message),
            StorageError::Other(source) => {
                tracing::error!("Storage error: {source}");
                AppError::Internal("internal server error".to_string())
            }
        }
    }
}

impl From<StoreError> for AppError {
    fn from(err: StoreError) -> Self {
        match &err {
            StoreError::NotFound => AppError::NotFound(err.to_string()),
            StoreError::Conflict => AppError::Conflict(CONFLICT_BODY.to_string()),
            StoreError::Unavailable | StoreError::Superseded(_) => {
                AppError::ServiceUnavailable(err.to_string())
            }
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

    /// The name validators answer with `DomainError` now; this pins the
    /// status and the body bytes they reach a client as, which is the whole
    /// of what the `AppError::BadRequest` they used to build guaranteed.
    #[tokio::test]
    async fn invalid_name_serves_what_validate_package_name_serves_today() {
        let refusal = crate::domain::validate_package_name("npm", "Bad Name").unwrap_err();
        assert_eq!(refusal.to_string(), "invalid npm package name: 'Bad Name'");
        assert!(matches!(refusal, DomainError::InvalidName(_)));

        assert_eq!(
            served(refusal.into()).await,
            (
                StatusCode::BAD_REQUEST,
                "invalid npm package name: 'Bad Name'".to_string()
            )
        );
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

    /// The other half of the guarantee is next to the guard itself, where
    /// every traversal attempt is asserted to be an `InvalidPath`: this side
    /// pins the status that variant is served as.
    #[tokio::test]
    async fn a_key_the_traversal_guard_rejects_is_still_a_400() {
        assert_eq!(
            served(StorageError::InvalidPath("path must not contain '..'".to_string()).into())
                .await,
            (
                StatusCode::BAD_REQUEST,
                "path must not contain '..'".to_string()
            )
        );
    }

    #[tokio::test]
    async fn storage_keeps_the_statuses_the_filesystem_backend_served() {
        assert_eq!(
            served(StorageError::NotFound.into()).await,
            (StatusCode::NOT_FOUND, "file not found".to_string())
        );
        assert_eq!(
            served(StorageError::Unavailable.into()).await,
            (
                StatusCode::SERVICE_UNAVAILABLE,
                "the storage backend is unavailable, try again".to_string()
            )
        );
        let io = std::io::Error::other("disk on fire");
        let (status, body) = served(StorageError::from(io).into()).await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(body, "internal server error");
    }

    #[tokio::test]
    async fn store_other_is_a_500_that_keeps_its_source_out_of_the_body() {
        let source = std::io::Error::other("/srv/data/db/opencargo.db is locked");
        let (status, body) = served(StoreError::Other(Box::new(source)).into()).await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(body, "internal server error");
    }
}
