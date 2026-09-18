//! `store_contract!`: what every adapter of a port owes the layers above,
//! asserted against each of them — the fake the unit tests run on and the
//! SQLite adapter the server runs on. A clause that holds for only one of
//! them is a clause about an adapter, not about the port.
//!
//! The suite reaches SQLite through `SqliteStores::open`, so it never names a
//! pool type: `scripts/boundary.sh`'s `tests/` row targets that spelling and
//! this module is its one written exclusion.

use std::any::Any;
use std::sync::Arc;

use opencargo::ports::webhooks::WebhookStore;

/// What an adapter hands the suite: its handles, plus whatever must outlive
/// them — a temp directory, an open pool — which the suite never looks at.
pub struct Handles {
    pub webhooks: Arc<dyn WebhookStore>,
    _keep: Box<dyn Any + Send>,
}

impl Handles {
    pub fn new(webhooks: Arc<dyn WebhookStore>, keep: Box<dyn Any + Send>) -> Self {
        Self {
            webhooks,
            _keep: keep,
        }
    }
}

/// `store_contract!(name, opener)` instantiates the suite against one
/// adapter; `opener` is an `async fn() -> Handles`.
///
/// The `allow`s are for the integration tests that pull `mod common;` in
/// without instantiating the suite, which is most of them.
#[allow(unused_macros)]
macro_rules! store_contract {
    ($suite:ident, $open:path) => {
        mod $suite {
            use super::*;
            use ::chrono::{DateTime, SubsecRound, TimeZone, Utc};
            use ::opencargo::domain::Subscription;
            use ::opencargo::error::StoreError;
            use ::opencargo::ports::webhooks::{NewWebhook, WebhookPatch};

            fn at(hour: u32) -> DateTime<Utc> {
                Utc.with_ymd_and_hms(2026, 9, 18, hour, 30, 0).unwrap()
            }

            /// The stored stamp back as a value, whichever spelling the
            /// adapter chose: the contract is about the instant, not about
            /// one rendering of it.
            fn stamped(raw: &str) -> DateTime<Utc> {
                ::opencargo::db::parse_ts(raw).expect("a stored timestamp reads back")
            }

            #[tokio::test]
            async fn a_registration_round_trips_through_the_store() {
                let handles = $open().await;
                let store = &handles.webhooks;
                let events = Subscription::Only(vec!["package.published".to_string()]);

                let created = store
                    .create(
                        &NewWebhook {
                            url: "https://example.com/hook",
                            events: &events,
                            secret: Some("s3cret"),
                        },
                        at(9),
                    )
                    .await
                    .unwrap();

                assert_eq!(created.url, "https://example.com/hook");
                assert_eq!(created.events, events);
                assert_eq!(created.secret.as_deref(), Some("s3cret"));
                assert!(created.active, "a new registration is live");

                assert_eq!(
                    store.by_id(created.id).await.unwrap(),
                    Some(created.clone())
                );
                assert_eq!(store.all().await.unwrap(), vec![created.clone()]);
                assert_eq!(store.active().await.unwrap(), vec![created]);
            }

            /// Section 1.5's clause: the row carries the timestamp the caller
            /// passed, never a column default fired by the server's clock.
            #[tokio::test]
            async fn a_row_carries_the_callers_clock() {
                let handles = $open().await;
                let store = &handles.webhooks;

                let created = store
                    .create(
                        &NewWebhook {
                            url: "https://example.com/hook",
                            events: &Subscription::All,
                            secret: None,
                        },
                        at(9),
                    )
                    .await
                    .unwrap();
                assert_eq!(stamped(&created.created_at), at(9).trunc_subsecs(0));
                assert_eq!(stamped(&created.updated_at), at(9).trunc_subsecs(0));

                let updated = store
                    .update(
                        created.id,
                        &WebhookPatch {
                            active: Some(false),
                            ..WebhookPatch::default()
                        },
                        at(11),
                    )
                    .await
                    .unwrap();
                assert_eq!(stamped(&updated.created_at), at(9).trunc_subsecs(0));
                assert_eq!(stamped(&updated.updated_at), at(11).trunc_subsecs(0));
            }

            #[tokio::test]
            async fn an_update_touches_only_the_fields_it_names() {
                let handles = $open().await;
                let store = &handles.webhooks;
                let events = Subscription::Only(vec!["package.promoted".to_string()]);

                let created = store
                    .create(
                        &NewWebhook {
                            url: "https://example.com/hook",
                            events: &events,
                            secret: Some("s3cret"),
                        },
                        at(9),
                    )
                    .await
                    .unwrap();

                let updated = store
                    .update(
                        created.id,
                        &WebhookPatch {
                            url: Some("https://example.com/hook-v2"),
                            ..WebhookPatch::default()
                        },
                        at(11),
                    )
                    .await
                    .unwrap();

                assert_eq!(updated.url, "https://example.com/hook-v2");
                assert_eq!(updated.events, events);
                assert_eq!(updated.secret.as_deref(), Some("s3cret"));
                assert!(updated.active);
            }

            /// Deactivating keeps the registration but takes it out of the
            /// set a delivery reads.
            #[tokio::test]
            async fn only_live_registrations_are_deliverable() {
                let handles = $open().await;
                let store = &handles.webhooks;

                let created = store
                    .create(
                        &NewWebhook {
                            url: "https://example.com/hook",
                            events: &Subscription::All,
                            secret: None,
                        },
                        at(9),
                    )
                    .await
                    .unwrap();

                store
                    .update(
                        created.id,
                        &WebhookPatch {
                            active: Some(false),
                            ..WebhookPatch::default()
                        },
                        at(11),
                    )
                    .await
                    .unwrap();

                assert!(store.active().await.unwrap().is_empty());
                assert_eq!(store.all().await.unwrap().len(), 1);
            }

            #[tokio::test]
            async fn a_registration_that_is_not_there_is_not_found() {
                let handles = $open().await;
                let store = &handles.webhooks;

                assert!(store.by_id(404).await.unwrap().is_none());
                assert!(matches!(
                    store
                        .update(404, &WebhookPatch::default(), at(9))
                        .await
                        .unwrap_err(),
                    StoreError::NotFound
                ));
                assert!(matches!(
                    store.delete(404).await.unwrap_err(),
                    StoreError::NotFound
                ));
            }

            #[tokio::test]
            async fn deleting_is_final_and_says_so_twice() {
                let handles = $open().await;
                let store = &handles.webhooks;

                let created = store
                    .create(
                        &NewWebhook {
                            url: "https://example.com/hook",
                            events: &Subscription::All,
                            secret: None,
                        },
                        at(9),
                    )
                    .await
                    .unwrap();

                store.delete(created.id).await.unwrap();
                assert!(store.by_id(created.id).await.unwrap().is_none());
                assert!(matches!(
                    store.delete(created.id).await.unwrap_err(),
                    StoreError::NotFound
                ));
            }

            /// The config file seeds a deployment; it does not own the
            /// registrations afterwards, so a second boot changes nothing.
            #[tokio::test]
            async fn seeding_fills_an_empty_store_once() {
                let handles = $open().await;
                let store = &handles.webhooks;
                let all = Subscription::All;

                store
                    .ensure_seeded(
                        &[
                            NewWebhook {
                                url: "https://example.com/first",
                                events: &all,
                                secret: None,
                            },
                            NewWebhook {
                                url: "https://example.com/second",
                                events: &all,
                                secret: None,
                            },
                        ],
                        at(9),
                    )
                    .await
                    .unwrap();
                assert_eq!(store.all().await.unwrap().len(), 2);

                store
                    .ensure_seeded(
                        &[NewWebhook {
                            url: "https://example.com/third",
                            events: &all,
                            secret: None,
                        }],
                        at(11),
                    )
                    .await
                    .unwrap();
                let registered = store.all().await.unwrap();
                assert_eq!(registered.len(), 2, "a second boot re-seeds nothing");
                assert_eq!(registered[0].url, "https://example.com/first");
            }
        }
    };
}

#[allow(unused_imports)]
pub(crate) use store_contract;
