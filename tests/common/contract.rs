//! `store_contract!`: what every adapter of a port owes the layers above,
//! asserted against each of them — the fake the unit tests run on and the
//! SQLite adapter the server runs on. A clause that holds for only one of
//! them is a clause about an adapter, not about the port.
//!
//! The suite reaches SQLite through `SqliteStores::open`, so it never names a
//! pool type: `scripts/boundary.sh`'s `tests/` row targets that spelling and
//! this module is its one written exclusion.

use std::any::Any;
use std::path::Path;
use std::sync::Arc;

use opencargo::adapters::sqlite::SqliteStores;
use opencargo::ports::proxy_cache::ProxyCacheStore;
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

/// The same, for `ProxyCacheStore`. A second struct rather than a second
/// field: an adapter proves one port at a time, and the suites are
/// instantiated from different test binaries.
pub struct CacheHandles {
    pub cache: Arc<dyn ProxyCacheStore>,
    _keep: Box<dyn Any + Send>,
}

impl CacheHandles {
    pub fn new(cache: Arc<dyn ProxyCacheStore>, keep: Box<dyn Any + Send>) -> Self {
        Self { cache, _keep: keep }
    }
}

/// A migrated SQLite database holding one proxy repository, for the ports
/// whose rows hang off a foreign key. Which adapter enforces that key is not
/// the contract's business, so the one statement that seeds it lives here —
/// in the `tests/` ratchet row's single written exclusion (4.2) — and nowhere
/// else in the suite.
pub async fn sqlite_with_repository(path: &Path) -> SqliteStores {
    let pool = opencargo::db::connect(&format!("sqlite:{}?mode=rwc", path.display()))
        .await
        .unwrap();
    opencargo::server::migrate(&pool).await.unwrap();
    sqlx::query(
        "INSERT INTO repositories (name, repo_type, format, upstream_url)
         VALUES ('p', 'proxy', 'npm', 'https://registry.npmjs.org')",
    )
    .execute(&pool)
    .await
    .unwrap();
    SqliteStores::new(pool)
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

/// `proxy_cache_contract!(name, opener)`: what the proxy relies on from any
/// cache store. Every clause states the caller's clock explicitly, because
/// that is the whole point of the port — no adapter may answer from its own.
#[allow(unused_macros)]
macro_rules! proxy_cache_contract {
    ($suite:ident, $open:path) => {
        mod $suite {
            use super::*;
            use ::chrono::{DateTime, SubsecRound, TimeDelta, TimeZone, Utc};
            use ::opencargo::domain::NewEntry;
            use ::std::time::Duration;

            const REPO: i64 = 1;

            fn at(hour: u32) -> DateTime<Utc> {
                Utc.with_ymd_and_hms(2026, 9, 18, hour, 30, 0).unwrap()
            }

            fn entry<'a>(key: &'a str, status: i64, ttl: Option<u64>) -> NewEntry<'a> {
                NewEntry {
                    repository_id: REPO,
                    kind: "npm-metadata",
                    cache_key: key,
                    status,
                    storage_path: (status == 200).then_some("_proxy_cache/p/x"),
                    content_type: Some("application/json"),
                    etag: Some("\"e1\""),
                    digest: Some("abc"),
                    size: 42,
                    ttl_secs: ttl,
                }
            }

            async fn read(
                store: &::std::sync::Arc<dyn ::opencargo::ports::proxy_cache::ProxyCacheStore>,
                key: &str,
                now: DateTime<Utc>,
            ) -> ::opencargo::domain::CacheEntry {
                store
                    .entry(REPO, "npm-metadata", key, now)
                    .await
                    .unwrap()
                    .expect("the row is there")
            }

            #[tokio::test]
            async fn an_answer_round_trips_with_the_clock_it_was_written_at() {
                let handles = $open().await;
                let store = &handles.cache;

                assert!(store
                    .entry(REPO, "npm-metadata", "lodash", at(9))
                    .await
                    .unwrap()
                    .is_none());

                store.upsert(&entry("lodash", 200, Some(3600)), at(9)).await.unwrap();
                let row = read(store, "lodash", at(9)).await;

                assert_eq!((row.status, row.size), (200, 42));
                assert_eq!(row.storage_path.as_deref(), Some("_proxy_cache/p/x"));
                assert_eq!(row.content_type.as_deref(), Some("application/json"));
                assert_eq!(row.etag.as_deref(), Some("\"e1\""));
                assert_eq!(row.digest.as_deref(), Some("abc"));
                // 1.5: the row carries the caller's instant, never a column
                // default fired by the server's clock.
                assert_eq!(row.fetched_at.trunc_subsecs(0), at(9).trunc_subsecs(0));
                assert_eq!(row.last_used_at.trunc_subsecs(0), at(9).trunc_subsecs(0));
                assert_eq!(
                    row.expires_at.map(|until| until.trunc_subsecs(0)),
                    Some((at(9) + TimeDelta::seconds(3600)).trunc_subsecs(0))
                );
            }

            /// The boundary case a `>` and a `>=` disagree on, and the one an
            /// adapter reading its own clock cannot be held to.
            #[tokio::test]
            async fn an_entry_is_stale_at_exactly_its_expiry() {
                let handles = $open().await;
                let store = &handles.cache;
                store.upsert(&entry("lodash", 200, Some(3600)), at(9)).await.unwrap();
                let expires = at(9) + TimeDelta::seconds(3600);

                assert!(read(store, "lodash", expires - TimeDelta::seconds(1)).await.fresh);
                assert!(!read(store, "lodash", expires).await.fresh);
                assert!(!read(store, "lodash", expires + TimeDelta::seconds(1)).await.fresh);
            }

            #[tokio::test]
            async fn an_immutable_answer_never_expires_and_upserts_in_place() {
                let handles = $open().await;
                let store = &handles.cache;
                store.upsert(&entry("lodash", 200, Some(60)), at(9)).await.unwrap();
                let first = read(store, "lodash", at(9)).await;

                store.upsert(&entry("lodash", 200, None), at(11)).await.unwrap();
                let again = read(store, "lodash", at(23)).await;

                assert_eq!(again.id, first.id, "one key is one row");
                assert_eq!(again.expires_at, None);
                assert!(again.fresh, "no expiry is an answer that never goes stale");
            }

            #[tokio::test]
            async fn a_touch_moves_the_use_and_only_a_revalidation_moves_the_expiry() {
                let handles = $open().await;
                let store = &handles.cache;
                store.upsert(&entry("lodash", 200, Some(3600)), at(9)).await.unwrap();
                let row = read(store, "lodash", at(9)).await;

                store.touch(row.id, None, at(10)).await.unwrap();
                let touched = read(store, "lodash", at(10)).await;
                assert_eq!(touched.last_used_at.trunc_subsecs(0), at(10).trunc_subsecs(0));
                assert_eq!(touched.expires_at, row.expires_at, "a hit is not a refresh");

                store
                    .touch(row.id, Some(Duration::from_secs(3600)), at(11))
                    .await
                    .unwrap();
                let revalidated = read(store, "lodash", at(11)).await;
                assert!(revalidated.fresh);
                assert_eq!(
                    revalidated.expires_at.map(|until| until.trunc_subsecs(0)),
                    Some((at(11) + TimeDelta::seconds(3600)).trunc_subsecs(0))
                );
            }

            /// What the sweep may take: an expired negative answer and
            /// anything unused for `idle` — never a stale positive row, which
            /// still serves while its upstream is down.
            #[tokio::test]
            async fn only_expired_negatives_and_unused_rows_are_evictable() {
                let handles = $open().await;
                let store = &handles.cache;
                let day = Duration::from_secs(86_400);
                store.upsert(&entry("idle", 200, None), at(9)).await.unwrap();
                store.upsert(&entry("gone", 404, Some(60)), at(9)).await.unwrap();
                store.upsert(&entry("stale", 200, Some(60)), at(9)).await.unwrap();
                store.upsert(&entry("fresh-negative", 404, Some(3600)), at(9)).await.unwrap();

                let now = at(9) + TimeDelta::seconds(1800);
                let soon: Vec<String> = store
                    .evictable(day, now, 10)
                    .await
                    .unwrap()
                    .into_iter()
                    .map(|row| row.cache_key)
                    .collect();
                assert_eq!(soon, vec!["gone".to_string()]);

                let later = at(9) + TimeDelta::days(2);
                let keys: Vec<String> = store
                    .evictable(day, later, 10)
                    .await
                    .unwrap()
                    .into_iter()
                    .map(|row| row.cache_key)
                    .collect();
                assert_eq!(keys.len(), 4, "everything is unused by then");
                assert_eq!(store.evictable(day, later, 2).await.unwrap().len(), 2);
            }

            #[tokio::test]
            async fn forgetting_is_by_row_or_by_repository() {
                let handles = $open().await;
                let store = &handles.cache;
                store.upsert(&entry("a", 200, Some(60)), at(9)).await.unwrap();
                store.upsert(&entry("b", 200, Some(60)), at(9)).await.unwrap();
                let a = read(store, "a", at(9)).await;

                store.delete(a.id).await.unwrap();
                assert!(store.entry(REPO, "npm-metadata", "a", at(9)).await.unwrap().is_none());
                store.delete(a.id).await.unwrap();

                assert_eq!(store.delete_for_repo(REPO).await.unwrap(), 1);
                assert!(store.entry(REPO, "npm-metadata", "b", at(9)).await.unwrap().is_none());
                assert_eq!(store.delete_for_repo(REPO).await.unwrap(), 0);
            }
        }
    };
}

#[allow(unused_imports)]
pub(crate) use proxy_cache_contract;
