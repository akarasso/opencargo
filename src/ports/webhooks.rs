//! The webhook registrations an operator keeps: the admin CRUD reads and
//! writes them, a publish reads the active ones to know where to deliver.
//!
//! Nothing here is coarse, because no two of these writes have to land
//! together: a registration is one row, and seeding is a boot-time list.

use async_trait::async_trait;
use chrono::{DateTime, Utc};

use crate::domain::{Subscription, Webhook};
use crate::error::StoreError;

/// A registration to create: borrowed in, owned out.
pub struct NewWebhook<'a> {
    pub url: &'a str,
    pub events: &'a Subscription,
    pub secret: Option<&'a str>,
}

/// Which fields an update touches. `None` leaves one alone, and
/// `secret: Some(None)` clears it — the distinction the admin API makes
/// between "not mentioned" and "set to null".
#[derive(Default)]
pub struct WebhookPatch<'a> {
    pub url: Option<&'a str>,
    pub events: Option<&'a Subscription>,
    pub secret: Option<Option<&'a str>>,
    pub active: Option<bool>,
}

impl WebhookPatch<'_> {
    pub fn touches_nothing(&self) -> bool {
        self.url.is_none()
            && self.events.is_none()
            && self.secret.is_none()
            && self.active.is_none()
    }
}

#[async_trait]
pub trait WebhookStore: Send + Sync {
    /// Every registration, active or not: the admin list.
    async fn all(&self) -> Result<Vec<Webhook>, StoreError>;

    async fn by_id(&self, id: i64) -> Result<Option<Webhook>, StoreError>;

    /// `now` fills `created_at` and `updated_at`, so no column default ever
    /// fires and the row carries the caller's clock.
    async fn create(
        &self,
        hook: &NewWebhook<'_>,
        now: DateTime<Utc>,
    ) -> Result<Webhook, StoreError>;

    /// `NotFound` if no such registration; a patch that touches nothing reads
    /// the row back without stamping it.
    async fn update(
        &self,
        id: i64,
        patch: &WebhookPatch<'_>,
        now: DateTime<Utc>,
    ) -> Result<Webhook, StoreError>;

    /// `NotFound` if it was already gone.
    async fn delete(&self, id: i64) -> Result<(), StoreError>;

    /// The registrations a delivery may reach. Which of them wants a given
    /// event is [`Subscription::matches`]'s answer, not a query's.
    async fn active(&self) -> Result<Vec<Webhook>, StoreError>;

    /// Insert the configured registrations, but only into an empty table: the
    /// config file seeds a deployment, it does not own it afterwards.
    async fn ensure_seeded(
        &self,
        hooks: &[NewWebhook<'_>],
        now: DateTime<Utc>,
    ) -> Result<(), StoreError>;
}
