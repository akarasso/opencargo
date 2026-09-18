use std::sync::Arc;

use chrono::{DateTime, Utc};

use crate::domain::{DomainError, Subscription, Webhook};
use crate::error::{AppError, StoreError};
use crate::ports::webhooks::{NewWebhook, WebhookStore};

/// How a webhook use case refuses: the caller's mistake, or the store's
/// failure. Keeping them apart is what stops a database outage from being
/// reported as a bad request.
#[derive(Debug, thiserror::Error)]
pub enum WebhookError {
    #[error(transparent)]
    Invalid(#[from] DomainError),

    #[error(transparent)]
    Store(#[from] StoreError),
}

impl From<WebhookError> for AppError {
    fn from(err: WebhookError) -> Self {
        match err {
            WebhookError::Invalid(err) => err.into(),
            WebhookError::Store(err) => err.into(),
        }
    }
}

/// What an operator asked to register.
pub struct NewHook {
    pub url: String,
    /// Empty means every event.
    pub events: Vec<String>,
    pub secret: Option<String>,
}

pub struct CreateWebhook {
    store: Arc<dyn WebhookStore>,
}

impl CreateWebhook {
    pub fn new(store: Arc<dyn WebhookStore>) -> Self {
        Self { store }
    }

    pub async fn run(&self, hook: &NewHook, now: DateTime<Utc>) -> Result<Webhook, WebhookError> {
        if hook.url.is_empty() {
            return Err(DomainError::InvalidName("url is required".to_string()).into());
        }
        let events = Subscription::of_names(&hook.events);
        let created = self
            .store
            .create(
                &NewWebhook {
                    url: &hook.url,
                    events: &events,
                    secret: hook.secret.as_deref(),
                },
                now,
            )
            .await?;
        Ok(created)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::fakes::{FakeDb, PortId};

    fn hook(url: &str, events: &[&str]) -> NewHook {
        NewHook {
            url: url.to_string(),
            events: events.iter().map(|e| e.to_string()).collect(),
            secret: None,
        }
    }

    fn now() -> DateTime<Utc> {
        DateTime::UNIX_EPOCH
    }

    #[tokio::test]
    async fn a_registration_comes_back_with_what_the_caller_described() {
        let db = FakeDb::new();
        let created = CreateWebhook::new(db.webhooks())
            .run(
                &hook("https://example.com/hook", &["package.published"]),
                now(),
            )
            .await
            .unwrap();

        assert_eq!(created.url, "https://example.com/hook");
        assert_eq!(
            created.events,
            Subscription::Only(vec!["package.published".to_string()])
        );
        assert!(created.active);
        assert_eq!(db.webhooks().all().await.unwrap(), vec![created]);
    }

    #[tokio::test]
    async fn naming_no_event_registers_for_everything() {
        let db = FakeDb::new();
        let created = CreateWebhook::new(db.webhooks())
            .run(&hook("https://example.com/hook", &[]), now())
            .await
            .unwrap();

        assert_eq!(created.events, Subscription::All);
        assert!(created.events.matches("package.promoted"));
    }

    #[tokio::test]
    async fn an_empty_url_is_refused_before_the_store_is_touched() {
        let db = FakeDb::new();
        let refused = CreateWebhook::new(db.webhooks())
            .run(&hook("", &[]), now())
            .await
            .unwrap_err();

        assert!(matches!(
            refused,
            WebhookError::Invalid(DomainError::InvalidName(_))
        ));
        assert!(db.webhooks().all().await.unwrap().is_empty());
    }

    /// A failing store is not the caller's fault: the distinction the two
    /// variants exist for, and the one a 400 would get wrong.
    #[tokio::test]
    async fn a_failing_store_is_not_a_bad_request() {
        let db = FakeDb::new();
        db.fail_next(PortId::Webhooks, StoreError::Unavailable);

        let refused = CreateWebhook::new(db.webhooks())
            .run(&hook("https://example.com/hook", &[]), now())
            .await
            .unwrap_err();

        assert!(matches!(
            refused,
            WebhookError::Store(StoreError::Unavailable)
        ));
        assert!(matches!(
            AppError::from(refused),
            AppError::ServiceUnavailable(_)
        ));
    }
}
