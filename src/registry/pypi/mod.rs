pub mod auth_rules;
pub mod leaves;
pub mod manage;
pub mod metadata;
pub mod multipart;
pub mod names;
pub mod read;
pub mod routes;
pub mod simple;
pub mod upload;
pub mod version;

use axum::http::{header, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};

use crate::app::publish::PublishError;
use crate::domain::DomainError;
use crate::error::{AppError, StoreError};
use crate::registry::resolve::ResolveError;

/// Seconds a client is told to wait before retrying a 503.
const RETRY_AFTER: &str = "5";

/// How a PyPI route refuses: the server's statuses, plus the 406 of PEP 691
/// negotiation, and every 503 carrying `Retry-After`.
#[derive(Debug)]
pub enum PypiError {
    App(AppError),
    NotAcceptable,
}

pub type PypiResult<T> = Result<T, PypiError>;

impl IntoResponse for PypiError {
    fn into_response(self) -> Response {
        match self {
            PypiError::NotAcceptable => (
                StatusCode::NOT_ACCEPTABLE,
                "no acceptable Simple API content type",
            )
                .into_response(),
            PypiError::App(err) => {
                let unavailable = matches!(err, AppError::ServiceUnavailable(_));
                let mut response = err.into_response();
                if unavailable {
                    response
                        .headers_mut()
                        .insert(header::RETRY_AFTER, HeaderValue::from_static(RETRY_AFTER));
                }
                response
            }
        }
    }
}

impl From<AppError> for PypiError {
    fn from(err: AppError) -> Self {
        PypiError::App(err)
    }
}

impl From<DomainError> for PypiError {
    fn from(err: DomainError) -> Self {
        PypiError::App(err.into())
    }
}

impl From<StoreError> for PypiError {
    fn from(err: StoreError) -> Self {
        PypiError::App(err.into())
    }
}

impl From<ResolveError> for PypiError {
    fn from(err: ResolveError) -> Self {
        PypiError::App(err.into())
    }
}

impl From<PublishError> for PypiError {
    fn from(err: PublishError) -> Self {
        PypiError::App(err.into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_503_says_when_to_retry_and_a_406_is_its_own() {
        let busy = PypiError::from(StoreError::Unavailable).into_response();
        assert_eq!(busy.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(busy.headers()[header::RETRY_AFTER], RETRY_AFTER);
        let missing = PypiError::from(AppError::NotFound("x".into())).into_response();
        assert!(missing.headers().get(header::RETRY_AFTER).is_none());
        assert_eq!(PypiError::NotAcceptable.into_response().status(), StatusCode::NOT_ACCEPTABLE);
    }
}
