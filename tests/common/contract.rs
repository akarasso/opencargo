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

use chrono::Utc;

use opencargo::adapters::sqlite::SqliteStores;
use opencargo::domain::{Format, RepoKind, RepoSpec, Visibility};
use opencargo::ports::audit::AuditStore;
use opencargo::ports::deps::DependencyStore;
use opencargo::ports::oci::OciStore;
use opencargo::ports::packages::PackageStore;
use opencargo::ports::policy::PolicyStore;
use opencargo::ports::proxy_cache::ProxyCacheStore;
use opencargo::ports::repositories::RepositoryStore;
use opencargo::ports::search::SearchIndex;
use opencargo::ports::vulns::VulnStore;
use opencargo::ports::webhooks::WebhookStore;

/// What an adapter hands the suite: its handles, plus whatever must outlive
/// them — a temp directory, an open pool — which the suite never looks at.
pub struct Handles {
    pub webhooks: Arc<dyn WebhookStore>,
    pub repos: Arc<dyn RepositoryStore>,
    pub packages: Arc<dyn PackageStore>,
    pub search: Arc<dyn SearchIndex>,
    _keep: Box<dyn Any + Send>,
}

/// The handle set an adapter exposes, which is the same set on both sides of
/// the boundary — that sameness is what lets one suite run against each.
pub struct Ports {
    pub webhooks: Arc<dyn WebhookStore>,
    pub repos: Arc<dyn RepositoryStore>,
    pub packages: Arc<dyn PackageStore>,
    pub search: Arc<dyn SearchIndex>,
}

impl Handles {
    pub fn new(ports: Ports, keep: Box<dyn Any + Send>) -> Self {
        Self {
            webhooks: ports.webhooks,
            repos: ports.repos,
            packages: ports.packages,
            search: ports.search,
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
/// whose rows hang off a foreign key. Seeded through `RepositoryStore`, which
/// step 7 ships: the suite states what it needs in the vocabulary of a port,
/// and no statement of its own.
pub async fn sqlite_with_repository(path: &Path) -> SqliteStores {
    let stores = SqliteStores::open(path).await.unwrap();
    stores
        .repositories()
        .ensure_seeded(
            &[RepoSpec {
                name: "p",
                kind: RepoKind::Proxy,
                format: Format::Npm,
                visibility: Visibility::Public,
                upstream: Some("https://registry.npmjs.org"),
                members: &[],
            }],
            Utc::now(),
        )
        .await
        .unwrap();
    stores
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
                assert_eq!(created.created_at, at(9).trunc_subsecs(0));
                assert_eq!(created.updated_at, at(9).trunc_subsecs(0));

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
                assert_eq!(updated.created_at, at(9).trunc_subsecs(0));
                assert_eq!(updated.updated_at, at(11).trunc_subsecs(0));
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

/// `package_contract!(name, opener)`: what `RepositoryStore`, `PackageStore`
/// and `SearchIndex` owe together.
///
/// Together, because they are one aggregate seen from three sides — a
/// publish is not landed until the version is readable *and* findable, and a
/// repository is not empty until its packages are gone. A per-port macro
/// structurally cannot say that.
#[allow(unused_macros)]
macro_rules! package_contract {
    ($suite:ident, $open:path) => {
        mod $suite {
            use super::*;
            use ::chrono::{DateTime, SubsecRound, TimeZone, Utc};
            use ::opencargo::domain::{Format, RepoKind, RepoSpec, Repository, Visibility};
            use ::opencargo::error::StoreError;
            use ::opencargo::ports::packages::{NameMatch, NewRelease};
            use ::opencargo::ports::search::{SearchQuery, SearchScope};

            fn at(hour: u32) -> DateTime<Utc> {
                Utc.with_ymd_and_hms(2026, 9, 18, hour, 30, 0).unwrap()
            }

            fn spec<'a>(name: &'a str, visibility: Visibility) -> RepoSpec<'a> {
                RepoSpec {
                    name,
                    kind: RepoKind::Hosted,
                    format: Format::Npm,
                    visibility,
                    upstream: None,
                    members: &[],
                }
            }

            async fn hosted(handles: &Handles, name: &str, visibility: Visibility) -> Repository {
                handles
                    .repos
                    .create(&spec(name, visibility), at(9))
                    .await
                    .unwrap()
            }

            fn release<'a>(
                repository: i64,
                package: &'a str,
                version: &'a str,
                tags: &'a [String],
            ) -> NewRelease<'a> {
                NewRelease {
                    repository,
                    package,
                    match_name: NameMatch::Exact,
                    description: Some("a package"),
                    readme: None,
                    version,
                    metadata_json: "{}",
                    checksum_sha1: None,
                    checksum_sha256: Some("abc"),
                    integrity: None,
                    size: 4,
                    tarball_path: "npm/r/p/p.tgz",
                    dist_tags: tags,
                    pins: &[],
                    now: at(9),
                }
            }

            #[tokio::test]
            async fn a_repository_round_trips_and_lists_by_name() {
                let handles = $open().await;
                let created = hosted(&handles, "npm-hosted", Visibility::Private).await;

                assert_eq!(created.name, "npm-hosted");
                assert_eq!(created.visibility, Visibility::Private);
                assert_eq!(created.kind().unwrap(), RepoKind::Hosted);
                assert_eq!(
                    handles.repos.by_name("npm-hosted").await.unwrap(),
                    Some(created.clone())
                );
                assert!(
                    handles.repos.by_name("NPM-HOSTED").await.unwrap().is_none(),
                    "the name is a storage segment, so it is matched exactly"
                );

                hosted(&handles, "a-first", Visibility::Public).await;
                let all = handles.repos.all().await.unwrap();
                assert_eq!(
                    all.iter().map(|r| r.name.as_str()).collect::<Vec<_>>(),
                    ["a-first", "npm-hosted"]
                );
            }

            /// Section 1.5's clause, on this aggregate too: the row carries
            /// the timestamp the caller passed, never a column default.
            #[tokio::test]
            async fn a_row_carries_the_callers_clock() {
                let handles = $open().await;
                let created = hosted(&handles, "npm-hosted", Visibility::Public).await;
                assert_eq!(created.created_at, at(9).trunc_subsecs(0));
                assert_eq!(created.updated_at, at(9).trunc_subsecs(0));

                let landed = handles
                    .packages
                    .publish_version(&release(created.id, "left-pad", "1.0.0", &[]))
                    .await
                    .unwrap();
                assert_eq!(landed.package.created_at, at(9).trunc_subsecs(0));
                assert_eq!(landed.version.published_at, at(9).trunc_subsecs(0));
            }

            #[tokio::test]
            async fn a_publish_lands_the_package_the_version_and_its_tags() {
                let handles = $open().await;
                let repo = hosted(&handles, "npm-hosted", Visibility::Public).await;
                let tags = vec!["latest".to_string(), "next".to_string()];

                let landed = handles
                    .packages
                    .publish_version(&release(repo.id, "left-pad", "1.0.0", &tags))
                    .await
                    .unwrap();

                assert_eq!(landed.package.name, "left-pad");
                assert_eq!(landed.version.version, "1.0.0");
                assert_eq!(landed.version.checksum_sha256.as_deref(), Some("abc"));
                assert!(!landed.version.yanked);

                let store = &handles.packages;
                assert_eq!(
                    store
                        .package(repo.id, "left-pad", NameMatch::Exact)
                        .await
                        .unwrap(),
                    Some(landed.package.clone())
                );
                assert_eq!(
                    store.versions(landed.package.id).await.unwrap(),
                    vec![landed.version.clone()]
                );
                let mut stored: Vec<String> = store
                    .dist_tags(landed.package.id)
                    .await
                    .unwrap()
                    .into_iter()
                    .map(|tag| tag.tag)
                    .collect();
                stored.sort();
                assert_eq!(stored, ["latest", "next"]);
            }

            /// The pre-insert read a caller does is an optimisation; this is
            /// the arbiter, and it must not be a 500.
            #[tokio::test]
            async fn two_concurrent_publishes_of_one_version_yield_one_conflict() {
                let handles = $open().await;
                let repo = hosted(&handles, "npm-hosted", Visibility::Public).await;
                let (left, right) = (handles.packages.clone(), handles.packages.clone());

                let (first, second) = ::tokio::join!(
                    async move {
                        left.publish_version(&release(repo.id, "left-pad", "1.0.0", &[]))
                            .await
                    },
                    async move {
                        right
                            .publish_version(&release(repo.id, "left-pad", "1.0.0", &[]))
                            .await
                    }
                );

                let outcomes = [first, second];
                assert_eq!(
                    outcomes.iter().filter(|out| out.is_ok()).count(),
                    1,
                    "exactly one of two racing publishes lands"
                );
                for out in &outcomes {
                    match out {
                        Ok(_) => {}
                        Err(StoreError::Conflict) => {}
                        Err(other) => panic!("a racing publish is a conflict, not {other:?}"),
                    }
                }
            }

            #[tokio::test]
            async fn a_case_insensitive_lookup_finds_the_case_it_was_published_with() {
                let handles = $open().await;
                let repo = hosted(&handles, "cargo-hosted", Visibility::Public).await;
                handles
                    .packages
                    .publish_version(&release(repo.id, "Serde", "1.0.0", &[]))
                    .await
                    .unwrap();

                let store = &handles.packages;
                assert!(store
                    .package(repo.id, "serde", NameMatch::Exact)
                    .await
                    .unwrap()
                    .is_none());
                assert_eq!(
                    store
                        .package(repo.id, "serde", NameMatch::Insensitive)
                        .await
                        .unwrap()
                        .map(|package| package.name),
                    Some("Serde".to_string()),
                    "the row keeps the case it was published with"
                );
            }

            /// A publish is not landed until it is findable: against SQLite
            /// this only means anything because `007_fts5.sql` applies
            /// strictly and the npm search has no `LIKE` fallback left.
            #[tokio::test]
            async fn a_published_package_is_findable_and_browsable() {
                let handles = $open().await;
                let repo = hosted(&handles, "npm-hosted", Visibility::Public).await;
                for name in ["left-pad", "right-pad"] {
                    handles
                        .packages
                        .publish_version(&release(repo.id, name, "1.0.0", &[]))
                        .await
                        .unwrap();
                }

                let query = SearchQuery::parse("left-pad").unwrap();
                let hits = handles
                    .search
                    .search(SearchScope::Repo(repo.id), Some(&query), 20)
                    .await
                    .unwrap();
                assert_eq!(
                    hits.first().map(|package| package.name.as_str()),
                    Some("left-pad"),
                    "the most relevant hit comes first"
                );

                let browsed = handles
                    .search
                    .search(SearchScope::Repo(repo.id), None, 20)
                    .await
                    .unwrap();
                let mut names: Vec<&str> =
                    browsed.iter().map(|package| package.name.as_str()).collect();
                names.sort();
                assert_eq!(
                    names,
                    ["left-pad", "right-pad"],
                    "no query is a browse, never an empty answer"
                );
            }

            #[tokio::test]
            async fn a_public_only_search_never_reaches_a_private_repository() {
                let handles = $open().await;
                let public = hosted(&handles, "npm-public", Visibility::Public).await;
                let private = hosted(&handles, "npm-private", Visibility::Private).await;
                for repo in [public.id, private.id] {
                    handles
                        .packages
                        .publish_version(&release(repo, "left-pad", "1.0.0", &[]))
                        .await
                        .unwrap();
                }

                let hits = handles
                    .search
                    .search(SearchScope::PublicOnly, None, 20)
                    .await
                    .unwrap();
                assert_eq!(hits.len(), 1);
                assert_eq!(hits[0].repository_id, public.id);

                let everywhere = handles
                    .search
                    .search(SearchScope::All, None, 20)
                    .await
                    .unwrap();
                assert_eq!(everywhere.len(), 2);
            }

            /// The cascade: a repository is not deletable while a package
            /// remains, and the refusal leaves it exactly where it was.
            #[tokio::test]
            async fn a_repository_with_a_package_refuses_to_go() {
                let handles = $open().await;
                let repo = hosted(&handles, "npm-hosted", Visibility::Public).await;
                handles
                    .packages
                    .publish_version(&release(repo.id, "left-pad", "1.0.0", &[]))
                    .await
                    .unwrap();

                assert!(matches!(
                    handles.repos.retire("npm-hosted", at(10)).await.unwrap_err(),
                    StoreError::Conflict
                ));
                assert_eq!(
                    handles.repos.by_name("npm-hosted").await.unwrap(),
                    Some(repo),
                    "a refused delete changes nothing"
                );
            }

            #[tokio::test]
            async fn deleting_an_empty_repository_is_final_and_says_so_twice() {
                let handles = $open().await;
                hosted(&handles, "npm-hosted", Visibility::Public).await;

                handles.repos.retire("npm-hosted", at(10)).await.unwrap();
                assert!(handles.repos.by_name("npm-hosted").await.unwrap().is_none());
                assert!(matches!(
                    handles.repos.retire("npm-hosted", at(10)).await.unwrap_err(),
                    StoreError::NotFound
                ));
            }

            /// A promotion is the whole of it or none of it: the package row
            /// in the target repository, the version, its inherited tags.
            #[tokio::test]
            async fn a_promotion_carries_the_version_and_the_tags_it_held() {
                use ::opencargo::ports::packages::{Promotion, PromotionAudit};

                let handles = $open().await;
                let stage = hosted(&handles, "npm-stage", Visibility::Private).await;
                let prod = hosted(&handles, "npm-prod", Visibility::Public).await;
                let tags = vec!["latest".to_string()];
                let landed = handles
                    .packages
                    .publish_version(&release(stage.id, "left-pad", "1.0.0", &tags))
                    .await
                    .unwrap();

                let promoted = handles
                    .packages
                    .promote_metadata(&Promotion {
                        source: &landed.version,
                        target_repository: prod.id,
                        package: "left-pad",
                        description: Some("a package"),
                        metadata_json: "{}",
                        tarball_path: "npm/npm-prod/left-pad/p.tgz",
                        dist_tags: &tags,
                        pins: &[],
                        audit: PromotionAudit {
                            user_id: None,
                            username: "alex",
                            target: "left-pad@1.0.0",
                            repository: "npm-prod",
                            details_json: "{}",
                        },
                        now: at(11),
                    })
                    .await
                    .unwrap();

                let store = &handles.packages;
                let target = store
                    .package(prod.id, "left-pad", NameMatch::Exact)
                    .await
                    .unwrap()
                    .expect("the target repository has the package row");
                assert_eq!(promoted.package_id, target.id);
                assert_eq!(promoted.tarball_path, "npm/npm-prod/left-pad/p.tgz");
                assert_eq!(promoted.published_at, at(11).trunc_subsecs(0));

                let inherited = store.dist_tags(target.id).await.unwrap();
                assert_eq!(inherited.len(), 1);
                assert_eq!(inherited[0].tag, "latest");
                assert_eq!(inherited[0].version_id, promoted.id);

                assert!(matches!(
                    store
                        .promote_metadata(&Promotion {
                            source: &landed.version,
                            target_repository: prod.id,
                            package: "left-pad",
                            description: None,
                            metadata_json: "{}",
                            tarball_path: "npm/npm-prod/left-pad/p.tgz",
                            dist_tags: &[],
                            pins: &[],
                            audit: PromotionAudit {
                                user_id: None,
                                username: "alex",
                                target: "left-pad@1.0.0",
                                repository: "npm-prod",
                                details_json: "{}",
                            },
                            now: at(12),
                        })
                        .await
                        .unwrap_err(),
                    StoreError::Conflict
                ));
            }

            #[tokio::test]
            async fn tags_metadata_and_yanking_touch_one_row_each() {
                let handles = $open().await;
                let repo = hosted(&handles, "npm-hosted", Visibility::Public).await;
                let landed = handles
                    .packages
                    .publish_version(&release(repo.id, "left-pad", "1.0.0", &[]))
                    .await
                    .unwrap();
                let store = &handles.packages;

                store
                    .set_dist_tag(landed.package.id, "latest", landed.version.id)
                    .await
                    .unwrap();
                assert_eq!(store.dist_tags(landed.package.id).await.unwrap().len(), 1);
                store
                    .clear_dist_tag(landed.package.id, "latest")
                    .await
                    .unwrap();
                assert!(store.dist_tags(landed.package.id).await.unwrap().is_empty());
                assert!(matches!(
                    store
                        .clear_dist_tag(landed.package.id, "latest")
                        .await
                        .unwrap_err(),
                    StoreError::NotFound
                ));

                store
                    .set_metadata(landed.version.id, r#"{"deprecated":"use left-pad2"}"#)
                    .await
                    .unwrap();
                store.set_yanked(landed.version.id, true).await.unwrap();
                store
                    .set_readme(landed.package.id, "# left-pad", at(11))
                    .await
                    .unwrap();

                let reread = store
                    .version(landed.package.id, "1.0.0")
                    .await
                    .unwrap()
                    .unwrap();
                assert_eq!(reread.metadata_json, r#"{"deprecated":"use left-pad2"}"#);
                assert!(reread.yanked);
                let package = store
                    .package(repo.id, "left-pad", NameMatch::Exact)
                    .await
                    .unwrap()
                    .unwrap();
                assert_eq!(package.readme.as_deref(), Some("# left-pad"));
                assert_eq!(package.updated_at, at(11).trunc_subsecs(0));
            }

            /// The delete cascade: none of the four foreign keys hanging off
            /// a version declares `ON DELETE CASCADE`, so the store owes the
            /// caller the whole of it — dist-tags, download rows and counter
            /// included. Against SQLite the counter is not merely asserted,
            /// it is enforced: `foreign_keys` is ON, so a `download_counts`
            /// row left behind makes the version delete fail outright.
            #[tokio::test]
            async fn deleting_a_version_takes_everything_hanging_off_it() {
                let handles = $open().await;
                let repo = hosted(&handles, "npm-hosted", Visibility::Public).await;
                let store = &handles.packages;
                let tags = vec!["latest".to_string()];
                let landed = store
                    .publish_version(&release(repo.id, "left-pad", "1.0.0", &tags))
                    .await
                    .unwrap();
                let kept = store
                    .publish_version(&release(repo.id, "left-pad", "1.1.0", &[]))
                    .await
                    .unwrap();
                store.record_download(landed.version.id).await.unwrap();

                store.delete_version(landed.version.id, at(10)).await.unwrap();

                assert!(store
                    .version(landed.package.id, "1.0.0")
                    .await
                    .unwrap()
                    .is_none());
                assert!(
                    store
                        .dist_tags(landed.package.id)
                        .await
                        .unwrap()
                        .is_empty(),
                    "the tag that pointed at it goes with it"
                );
                assert_eq!(
                    store
                        .versions(landed.package.id)
                        .await
                        .unwrap()
                        .iter()
                        .map(|v| v.id)
                        .collect::<Vec<_>>(),
                    [kept.version.id],
                    "a sibling version is untouched"
                );
                assert!(matches!(
                    store.delete_version(landed.version.id, at(10)).await.unwrap_err(),
                    StoreError::NotFound
                ));
            }

            /// The retention predicate reads the caller's clock and nothing
            /// else, and only the two formats whose `-` marks a pre-release.
            #[tokio::test]
            async fn the_sweep_sees_aged_pre_releases_of_npm_and_cargo_only() {
                use ::std::time::Duration;

                let handles = $open().await;
                let npm = hosted(&handles, "npm-hosted", Visibility::Public).await;
                let go = handles
                    .repos
                    .create(
                        &RepoSpec {
                            format: Format::Go,
                            ..spec("go-hosted", Visibility::Public)
                        },
                        at(9),
                    )
                    .await
                    .unwrap();
                let store = &handles.packages;
                let aged = store
                    .publish_version(&release(npm.id, "left-pad", "1.0.0-beta", &[]))
                    .await
                    .unwrap();
                store
                    .publish_version(&release(npm.id, "left-pad", "1.0.0", &[]))
                    .await
                    .unwrap();
                store
                    .publish_version(&release(
                        go.id,
                        "example.com/m",
                        "v0.0.0-20200101000000-abcdef",
                        &[],
                    ))
                    .await
                    .unwrap();

                let day = Duration::from_secs(86_400);
                let past_it = at(9) + day + Duration::from_secs(1);
                let stale = store.stale_prereleases(day, past_it).await.unwrap();
                assert_eq!(
                    stale.iter().map(|s| s.id).collect::<Vec<_>>(),
                    [aged.version.id],
                    "a release version and a go pseudo-version are not pre-releases to sweep"
                );
                assert_eq!(stale[0].package, "left-pad");
                assert_eq!(stale[0].version, "1.0.0-beta");
                assert_eq!(stale[0].tarball_path, "npm/r/p/p.tgz");

                assert!(
                    store
                        .stale_prereleases(day, at(9) + day)
                        .await
                        .unwrap()
                        .is_empty(),
                    "a version exactly at its bound is still inside it, on the caller's clock"
                );
            }

            #[tokio::test]
            async fn what_is_not_there_is_not_found() {
                let handles = $open().await;
                let store = &handles.packages;

                assert!(store.versions(404).await.unwrap().is_empty());
                assert!(store.version(404, "1.0.0").await.unwrap().is_none());
                assert!(store.dist_tags(404).await.unwrap().is_empty());
                assert!(matches!(
                    store.set_metadata(404, "{}").await.unwrap_err(),
                    StoreError::NotFound
                ));
                assert!(matches!(
                    store.set_yanked(404, true).await.unwrap_err(),
                    StoreError::NotFound
                ));
                assert!(matches!(
                    store.set_readme(404, "#", at(9)).await.unwrap_err(),
                    StoreError::NotFound
                ));
            }
        }
    };
}

#[allow(unused_imports)]
pub(crate) use package_contract;

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

/// What the tail's five stores hand the suite. A separate handle set, like
/// `CacheHandles`: an adapter proves one aggregate at a time.
pub struct TailHandles {
    pub policy: Arc<dyn PolicyStore>,
    pub audit: Arc<dyn AuditStore>,
    pub deps: Arc<dyn DependencyStore>,
    pub vulns: Arc<dyn VulnStore>,
    pub oci: Arc<dyn OciStore>,
    pub reclaim: Arc<dyn opencargo::ports::reclaim::ReclaimStore>,
    /// The repository an image's rows hang off; `OciStore` keys on it and
    /// carries no release of its own.
    pub repository: i64,
    /// Its incarnation prefix, the root of every key its pins name.
    pub prefix: String,
    /// A published version both the graph and the scan record hang off: the
    /// schema declares those foreign keys, so the suite states what it needs
    /// through `PackageStore` rather than writing orphan rows.
    pub release: Release,
    _keep: Box<dyn Any + Send>,
}

/// The package and version the tail's rows point at.
#[derive(Clone, Copy)]
pub struct Release {
    pub package: i64,
    pub version: i64,
}

/// The tail's handles as an adapter exposes them.
pub struct TailPorts {
    pub policy: Arc<dyn PolicyStore>,
    pub audit: Arc<dyn AuditStore>,
    pub deps: Arc<dyn DependencyStore>,
    pub vulns: Arc<dyn VulnStore>,
    pub oci: Arc<dyn OciStore>,
    pub reclaim: Arc<dyn opencargo::ports::reclaim::ReclaimStore>,
    pub repository: i64,
    pub prefix: String,
    pub release: Release,
}

impl TailHandles {
    pub fn new(ports: TailPorts, keep: Box<dyn Any + Send>) -> Self {
        Self {
            policy: ports.policy,
            audit: ports.audit,
            deps: ports.deps,
            vulns: ports.vulns,
            oci: ports.oci,
            reclaim: ports.reclaim,
            repository: ports.repository,
            prefix: ports.prefix,
            release: ports.release,
            _keep: keep,
        }
    }
}

/// `cascade_contract!(name, opener)`: what the report, the trail, the graph,
/// the scan record and an image's manifests owe together.
///
/// Together, because what they share is a rule rather than a table: every
/// row carries the clock its caller passed, and every deletion is decided by
/// identity — `user_id`, a cut-off instant, a version id, a manifest digest —
/// never by a label or by the database's own idea of the time. The manifest
/// cascade is here because it is the other deletion that decides what it
/// takes with it, and the orphan set it reports is the whole of what the
/// layer above acts on.
#[allow(unused_macros)]
macro_rules! cascade_contract {
    ($suite:ident, $open:path) => {
        mod $suite {
            use super::*;
            use ::chrono::{DateTime, SubsecRound, TimeZone, Utc};
            use ::opencargo::domain::{RuleVerdict, ScanResult, Verdict, VulnDetail};
            use ::opencargo::ports::audit::NewAuditEntry;
            use ::opencargo::ports::deps::NewDependency;
            use ::opencargo::error::StoreError;
            use ::opencargo::ports::oci::{
                BeginComplete, Finished, LeaseToken, NewBlob, NewManifest, Orphaned, Segment,
                SegmentClaim,
            };
            use ::opencargo::ports::reclaim::{PinToken, Pinned};
            use ::opencargo::ports::policy::{NewResolution, ReportFilter};

            fn at(hour: u32) -> DateTime<Utc> {
                Utc.with_ymd_and_hms(2026, 9, 18, hour, 30, 0).unwrap()
            }

            fn window() -> ReportFilter<'static> {
                ReportFilter {
                    since: DateTime::UNIX_EPOCH,
                    repo: None,
                    rule: None,
                    subject: None,
                }
            }

            fn verdicts() -> Vec<RuleVerdict> {
                vec![RuleVerdict::new("typosquat", Verdict::WouldBlock, "looks like left-pad")]
            }

            /// Two callers spelled the same and identified differently: the
            /// label is display, `user_id` is the identity.
            fn resolution<'a>(verdicts: &'a [RuleVerdict], user: i64) -> NewResolution<'a> {
                NewResolution {
                    requested_repo: "npm-all",
                    member_repo: "npm-proxy",
                    format: "npm",
                    name: "left-pad",
                    version: Some("1.0.0"),
                    digest: None,
                    published_at: Some(at(8)),
                    date_source: "fetch",
                    actor: "ci",
                    actor_kind: "token",
                    user_id: Some(user),
                    verdicts,
                }
            }

            /// 1.5: a row carries the timestamp the caller passed, never a
            /// column default fired by the server's clock.
            #[tokio::test]
            async fn a_resolution_carries_the_callers_clock_and_its_verdicts() {
                let handles = $open().await;
                let verdicts = verdicts();
                let ids = handles
                    .policy
                    .insert_batch(&[resolution(&verdicts, 7)], at(9))
                    .await
                    .unwrap();
                assert_eq!(ids.len(), 1);

                let listed = handles.policy.resolutions(&window(), 1, 10).await.unwrap();
                assert_eq!(listed.len(), 1);
                assert_eq!(
                    listed[0].created_at.trunc_subsecs(0),
                    at(9).trunc_subsecs(0)
                );
                assert_eq!(
                    listed[0].published_at.map(|t| t.trunc_subsecs(0)),
                    Some(at(8).trunc_subsecs(0))
                );
                assert_eq!(listed[0].date_source, "fetch");
                assert!(listed[0].would_block);
                assert_eq!(handles.policy.max_id().await.unwrap(), listed[0].id);

                let stored = handles
                    .policy
                    .verdicts_for(&[listed[0].id], None)
                    .await
                    .unwrap();
                assert_eq!(stored.len(), 1);
                assert_eq!(
                    (stored[0].rule.as_str(), stored[0].verdict.as_str()),
                    ("typosquat", "would_block")
                );
            }

            /// Erasure is by identity, never by label, and it takes the
            /// verdicts with the rows: a homonymous caller keeps theirs.
            #[tokio::test]
            async fn erasure_takes_one_identity_and_leaves_its_homonym() {
                let handles = $open().await;
                let verdicts = verdicts();
                handles
                    .policy
                    .insert_batch(
                        &[resolution(&verdicts, 7), resolution(&verdicts, 8)],
                        at(9),
                    )
                    .await
                    .unwrap();

                assert_eq!(handles.policy.erase_user(7).await.unwrap(), 1);

                let left = handles.policy.resolutions(&window(), 1, 10).await.unwrap();
                assert_eq!(left.len(), 1);
                assert_eq!(left[0].user_id, Some(8), "the homonym keeps its rows");
                assert_eq!(
                    left[0].actor, "ci",
                    "both were spelled the same; only the id decided"
                );
                let ids: Vec<i64> = left.iter().map(|r| r.id).collect();
                assert_eq!(
                    handles.policy.verdicts_for(&ids, None).await.unwrap().len(),
                    1,
                    "the erased row's verdicts went with it, and only those"
                );
            }

            /// Retention is the caller's cut-off, so a store that read its own
            /// clock would answer differently here.
            #[tokio::test]
            async fn retention_deletes_by_the_callers_cut_off() {
                let handles = $open().await;
                let verdicts = verdicts();
                handles
                    .policy
                    .insert_batch(&[resolution(&verdicts, 7)], at(9) - ::chrono::Duration::days(100))
                    .await
                    .unwrap();
                handles
                    .policy
                    .insert_batch(&[resolution(&verdicts, 7)], at(9))
                    .await
                    .unwrap();

                assert_eq!(handles.policy.delete_older_than(30, at(9)).await.unwrap(), 1);
                assert_eq!(
                    handles.policy.resolutions(&window(), 1, 10).await.unwrap().len(),
                    1
                );
                assert_eq!(handles.policy.delete_older_than(30, at(9)).await.unwrap(), 0);
            }

            /// The trail is read back newest first, under the caller's clock,
            /// and `of_target` answers about one action on one target.
            #[tokio::test]
            async fn the_trail_reads_back_newest_first_under_the_callers_clock() {
                let handles = $open().await;
                for (hour, action) in [(9, "user.create"), (10, "user.delete")] {
                    handles
                        .audit
                        .append(
                            &NewAuditEntry {
                                user_id: None,
                                username: Some("ci"),
                                action,
                                target: Some("bob"),
                                repository: None,
                                ip: None,
                                user_agent: None,
                                details_json: None,
                            },
                            at(hour),
                        )
                        .await
                        .unwrap();
                }

                let listed = handles.audit.recent(1, 10).await.unwrap();
                assert_eq!(
                    listed.iter().map(|e| e.action.as_str()).collect::<Vec<_>>(),
                    vec!["user.delete", "user.create"],
                    "newest first"
                );
                assert_eq!(listed[0].created_at.trunc_subsecs(0), at(10).trunc_subsecs(0));
                assert_eq!(listed[0].username.as_deref(), Some("ci"));
                assert_eq!(handles.audit.recent(2, 1).await.unwrap().len(), 1);

                let deletes = handles.audit.of_target("user.delete", "bob").await.unwrap();
                assert_eq!(deletes.len(), 1);
                assert!(handles
                    .audit
                    .of_target("user.delete", "carol")
                    .await
                    .unwrap()
                    .is_empty());
            }

            /// A scan is a fact about a moment, and rescanning starts from
            /// none: `forget` leaves the version unscanned, not clean.
            #[tokio::test]
            async fn a_scan_reads_back_and_a_forget_leaves_no_verdict() {
                let handles = $open().await;
                let result = ScanResult {
                    total_deps: 3,
                    vulnerable_deps: 1,
                    status: "warning".to_string(),
                    details: vec![VulnDetail {
                        dependency: "left-pad".to_string(),
                        version: "1.0.0".to_string(),
                        vuln_id: "GHSA-x".to_string(),
                        summary: "bad".to_string(),
                        severity: ::opencargo::domain::Severity::High,
                        score: Some(7.5),
                    }],
                };

                assert!(handles.vulns.latest(handles.release.version).await.unwrap().is_none());
                handles.vulns.record(handles.release.version, &result, at(9)).await.unwrap();

                let stored = handles.vulns.latest(handles.release.version).await.unwrap().unwrap();
                assert_eq!(stored.scanned_at.trunc_subsecs(0), at(9).trunc_subsecs(0));
                assert_eq!((stored.total_deps, stored.vulnerable_deps), (3, 1));
                assert_eq!(stored.status, "warning");
                assert_eq!(
                    stored.details.as_ref().and_then(|d| d.get(0)).and_then(|d| d["vuln_id"].as_str()),
                    Some("GHSA-x")
                );

                handles.vulns.forget(handles.release.version).await.unwrap();
                assert!(handles.vulns.latest(handles.release.version).await.unwrap().is_none());
                handles.vulns.forget(handles.release.version).await.unwrap();
            }

            /// The graph is edges out of a version, in the order recorded.
            #[tokio::test]
            async fn a_versions_edges_come_back_in_the_order_they_were_recorded() {
                let handles = $open().await;
                for (name, requirement, kind) in [
                    ("left-pad", "^1.0.0", "runtime"),
                    ("tape", "^5.0.0", "dev"),
                ] {
                    handles
                        .deps
                        .record(
                            &NewDependency {
                                package: handles.release.package,
                                version: handles.release.version,
                                name,
                                requirement,
                                kind,
                            },
                            at(9),
                        )
                        .await
                        .unwrap();
                }

                let edges = handles.deps.of_version(handles.release.version).await.unwrap();
                assert_eq!(
                    edges
                        .iter()
                        .map(|d| (d.name.as_str(), d.requirement.as_str(), d.kind.as_str()))
                        .collect::<Vec<_>>(),
                    vec![
                        ("left-pad", "^1.0.0", "runtime"),
                        ("tape", "^5.0.0", "dev"),
                    ]
                );
                assert!(handles.deps.of_version(handles.release.version + 1).await.unwrap().is_empty());
            }

            const LEASE: ::std::time::Duration = ::std::time::Duration::from_secs(60);

            async fn pins(handles: &TailHandles, keys: &[&str]) -> Vec<PinToken> {
                let keys: Vec<String> = keys.iter().map(|k| format!("{}/{k}", handles.prefix)).collect();
                match handles.reclaim.pin(&handles.prefix, &keys, at(23)).await.unwrap() {
                    Pinned::Tokens(tokens) => tokens,
                    Pinned::Retired => panic!("the repository is live"),
                }
            }

            fn new_blob(repository: i64, digest: &str) -> NewBlob<'_> {
                NewBlob {
                    repository,
                    digest,
                    size: 5,
                    content_type: "application/octet-stream",
                }
            }

            async fn session(handles: &TailHandles, id: &str) -> LeaseToken {
                let prefix = format!("{}/_uploads/{id}", handles.prefix);
                handles.oci.start_upload(id, handles.repository, "app", &prefix, at(8)).await.unwrap();
                match handles.oci.begin_complete(id, at(8), LEASE).await.unwrap() {
                    BeginComplete::Lease(lease) => lease,
                    other => panic!("a fresh session is free: {other:?}"),
                }
            }

            /// A blob as a completed upload records it: its row names the
            /// pinned key.
            async fn blob(handles: &TailHandles, digest: &str) -> String {
                let lease = session(handles, digest).await;
                let pin = pins(handles, &[digest]).await.remove(0);
                let finished = handles
                    .oci
                    .finish_upload(digest, &lease, &pin, new_blob(handles.repository, digest), at(8))
                    .await
                    .unwrap();
                assert_eq!(finished, Finished::Recorded(pin.physical_key.clone()));
                pin.physical_key
            }

            async fn put(handles: &TailHandles, digest: &str, blobs: &[String], tag: Option<&str>) -> Result<String, StoreError> {
                let mut keys = vec![digest];
                keys.extend(blobs.iter().map(String::as_str));
                let mut tokens = pins(handles, &keys).await;
                let pin = tokens.remove(0);
                let pinned: Vec<(String, PinToken)> = blobs.iter().cloned().zip(tokens).collect();
                handles
                    .oci
                    .put_manifest(
                        NewManifest {
                            repository: handles.repository,
                            name: "app",
                            digest,
                            content_type: "application/vnd.oci.image.manifest.v1+json",
                            size: 2,
                            pin: &pin,
                            blobs: &pinned,
                            children: &[],
                            tag,
                        },
                        at(9),
                    )
                    .await
                    .map(|()| pin.physical_key)
            }

            async fn queued(handles: &TailHandles) -> Vec<String> {
                let mut keys: Vec<String> = handles
                    .reclaim
                    .due(::std::time::Duration::ZERO, at(23), 100)
                    .await
                    .unwrap()
                    .into_iter()
                    .map(|c| c.key)
                    .collect();
                keys.sort();
                keys
            }

            /// The orphan set is the point of the method: a layer another
            /// manifest still lists is not in it, and a layer nothing lists
            /// any more is — its key enqueued with the manifest's, in the
            /// same transaction, and nothing deleted (M2).
            #[tokio::test]
            async fn a_manifest_delete_enqueues_only_the_layers_it_orphaned() {
                let handles = $open().await;
                let repo = handles.repository;
                let (shared, only) = ("sha256:cc".to_string(), "sha256:bb".to_string());
                let only_key = blob(&handles, &only).await;
                blob(&handles, &shared).await;
                let manifest_key = put(&handles, "sha256:aa", &[only.clone(), shared.clone()], Some("v1")).await.unwrap();
                put(&handles, "sha256:dd", std::slice::from_ref(&shared), Some("v2")).await.unwrap();
                assert!(queued(&handles).await.is_empty());

                let orphaned = handles
                    .oci
                    .delete_manifest(repo, "app", "sha256:aa", at(10))
                    .await
                    .unwrap()
                    .expect("the manifest was there");

                assert_eq!(orphaned, Orphaned { blob_digests: vec![only.clone()] });
                let mut want = vec![manifest_key, only_key];
                want.sort();
                assert_eq!(queued(&handles).await, want);
                assert!(handles.oci.manifest(repo, "app", "sha256:aa").await.unwrap().is_none());
                assert!(handles.oci.blob(repo, &only).await.unwrap().is_none());
                assert!(handles.oci.digest_for_ref(repo, "app", "v1").await.unwrap().is_none());
                assert_eq!(handles.oci.blob_references(repo, &shared).await.unwrap(), 1);
                assert_eq!(handles.oci.tags(repo, "app").await.unwrap(), vec!["v2".to_string()]);
            }

            /// Nothing to delete is `None`, not an empty orphan set, and it
            /// enqueues nothing.
            #[tokio::test]
            async fn deleting_an_unknown_manifest_reports_nothing() {
                let handles = $open().await;
                assert!(handles
                    .oci
                    .delete_manifest(handles.repository, "app", "sha256:aa", at(10))
                    .await
                    .unwrap()
                    .is_none());
                assert!(queued(&handles).await.is_empty());
            }

            /// A re-push replaces the link rows wholesale, so a layer the new
            /// manifest no longer lists stops being referenced.
            #[tokio::test]
            async fn a_re_push_replaces_the_layers_and_moves_the_tag() {
                let handles = $open().await;
                let repo = handles.repository;
                let (old, new) = ("sha256:bb".to_string(), "sha256:cc".to_string());
                blob(&handles, &old).await;
                blob(&handles, &new).await;
                put(&handles, "sha256:aa", std::slice::from_ref(&old), Some("v1")).await.unwrap();
                put(&handles, "sha256:aa", std::slice::from_ref(&new), Some("v1")).await.unwrap();

                assert_eq!(handles.oci.blob_references(repo, &old).await.unwrap(), 0);
                assert_eq!(handles.oci.blob_references(repo, &new).await.unwrap(), 1);
                assert_eq!(handles.oci.tags(repo, "app").await.unwrap(), vec!["v1".to_string()]);
            }

            /// A pin is not proof of existence: a manifest listing a blob
            /// with no row writes nothing (N4), and neither does one whose
            /// pin was revoked (M1).
            #[tokio::test]
            async fn put_manifest_with_an_unknown_blob_or_a_revoked_pin_writes_nothing() {
                let handles = $open().await;
                let repo = handles.repository;
                let unknown = put(&handles, "sha256:aa", &["sha256:bb".to_string()], Some("v1")).await;
                assert!(matches!(unknown, Err(StoreError::NotFound)), "{unknown:?}");
                assert!(handles.oci.manifest(repo, "app", "sha256:aa").await.unwrap().is_none());

                let pin = pins(&handles, &["sha256:aa"]).await.remove(0);
                handles.reclaim.enqueue(std::slice::from_ref(&pin.physical_key), at(1)).await.unwrap();
                let late = at(23) + ::chrono::Duration::days(2);
                handles.reclaim.claim(&pin.physical_key, ::std::time::Duration::from_secs(1), late, late).await.unwrap();
                let revoked = handles
                    .oci
                    .put_manifest(
                        NewManifest {
                            repository: repo,
                            name: "app",
                            digest: "sha256:aa",
                            content_type: "application/json",
                            size: 2,
                            pin: &pin,
                            blobs: &[],
                            children: &[],
                            tag: Some("v1"),
                        },
                        at(9),
                    )
                    .await;
                assert!(matches!(revoked, Err(StoreError::Superseded(ref k)) if *k == vec![pin.physical_key.clone()]), "{revoked:?}");
                assert!(handles.oci.manifest(repo, "app", "sha256:aa").await.unwrap().is_none());
                assert!(handles.oci.digest_for_ref(repo, "app", "v1").await.unwrap().is_none());
            }

            /// Chunks claim their offset by compare-and-set: one winner per
            /// offset, none while a completion holds the lease, and a cap.
            #[tokio::test]
            async fn a_segment_offset_has_one_winner() {
                let handles = $open().await;
                let prefix = format!("{}/_uploads/u1", handles.prefix);
                handles.oci.start_upload("u1", handles.repository, "app", &prefix, at(8)).await.unwrap();
                let seg = |start: u64, n: &str| Segment { start, len: 3, key: format!("{prefix}/{start:020}-{n}") };
                assert_eq!(handles.oci.claim_segment("u1", &seg(0, "a"), 2, at(8)).await.unwrap(), SegmentClaim::Won);
                assert_eq!(handles.oci.claim_segment("u1", &seg(0, "b"), 2, at(8)).await.unwrap(), SegmentClaim::Lost);
                assert_eq!(handles.oci.claim_segment("u1", &seg(3, "c"), 2, at(8)).await.unwrap(), SegmentClaim::Won);
                assert_eq!(handles.oci.claim_segment("u1", &seg(6, "d"), 2, at(8)).await.unwrap(), SegmentClaim::TooManySegments);
                assert_eq!(handles.oci.claim_segment("nope", &seg(0, "e"), 2, at(8)).await.unwrap(), SegmentClaim::Lost);
                let session = handles.oci.upload("u1").await.unwrap().unwrap();
                assert_eq!((session.received, session.segments, session.prefix.as_str()), (6, 2, prefix.as_str()));
                assert_eq!(handles.oci.segments("u1").await.unwrap(), vec![seg(0, "a"), seg(3, "c")]);

                let BeginComplete::Lease(_) = handles.oci.begin_complete("u1", at(9), LEASE).await.unwrap() else {
                    panic!("free");
                };
                assert_eq!(handles.oci.claim_segment("u1", &seg(6, "f"), 9, at(9)).await.unwrap(), SegmentClaim::Lost);
            }

            /// The lease is dated: held while live, taken over once expired
            /// with a fresh token, released only by its own token.
            #[tokio::test]
            async fn completion_claim_survives_a_crash() {
                let handles = $open().await;
                let first = session(&handles, "u1").await;
                assert_eq!(handles.oci.begin_complete("u1", at(8), LEASE).await.unwrap(), BeginComplete::Held);
                let BeginComplete::Lease(second) = handles.oci.begin_complete("u1", at(9), LEASE).await.unwrap() else {
                    panic!("an expired lease is taken over");
                };
                assert_ne!(first, second);
                handles.oci.release_complete("u1", &first).await.unwrap();
                assert_eq!(handles.oci.begin_complete("u1", at(9), LEASE).await.unwrap(), BeginComplete::Held);
                handles.oci.release_complete("u1", &second).await.unwrap();
                assert!(matches!(handles.oci.begin_complete("u1", at(9), LEASE).await.unwrap(), BeginComplete::Lease(_)));
                assert_eq!(handles.oci.begin_complete("nope", at(9), LEASE).await.unwrap(), BeginComplete::Unknown);
            }

            /// `Superseded(Lease)` and `Superseded(Pin)`: either writes
            /// nothing, the session and its row untouched.
            #[tokio::test]
            async fn finish_upload_with_a_lost_lease_or_a_revoked_pin_writes_nothing() {
                let handles = $open().await;
                let repo = handles.repository;
                let stale = session(&handles, "u1").await;
                let BeginComplete::Lease(live) = handles.oci.begin_complete("u1", at(10), LEASE).await.unwrap() else {
                    panic!("expired");
                };
                let pin = pins(&handles, &["sha256:bb"]).await.remove(0);
                let lost = handles.oci.finish_upload("u1", &stale, &pin, new_blob(repo, "sha256:bb"), at(10)).await.unwrap();
                assert_eq!(lost, Finished::LeaseLost);
                assert!(handles.oci.blob(repo, "sha256:bb").await.unwrap().is_none());

                handles.reclaim.enqueue(std::slice::from_ref(&pin.physical_key), at(1)).await.unwrap();
                let late = at(23) + ::chrono::Duration::days(2);
                handles.reclaim.claim(&pin.physical_key, ::std::time::Duration::from_secs(1), late, late).await.unwrap();
                let revoked = handles.oci.finish_upload("u1", &live, &pin, new_blob(repo, "sha256:bb"), at(10)).await;
                assert!(matches!(revoked, Err(StoreError::Superseded(_))), "{revoked:?}");
                assert!(handles.oci.blob(repo, "sha256:bb").await.unwrap().is_none());
                assert!(handles.oci.upload("u1").await.unwrap().is_some());
            }

            /// Two completions of one digest: the second keeps the recorded
            /// key and its own generation is enqueued (N9). The session goes
            /// with its segments either way.
            #[tokio::test]
            async fn a_second_completion_keeps_the_recorded_key_and_enqueues_its_own() {
                let handles = $open().await;
                let repo = handles.repository;
                let first = blob(&handles, "sha256:bb").await;
                let lease = session(&handles, "u2").await;
                let fresh = pins(&handles, &["other"]).await.remove(0);
                let finished = handles.oci.finish_upload("u2", &lease, &fresh, new_blob(repo, "sha256:bb"), at(9)).await.unwrap();
                assert_eq!(finished, Finished::Recorded(first.clone()));
                assert_eq!(handles.oci.blob(repo, "sha256:bb").await.unwrap().unwrap().key, first);
                assert_eq!(queued(&handles).await, vec![fresh.physical_key]);
                assert!(handles.oci.upload("u2").await.unwrap().is_none());
            }

            /// A listed blob refuses deletion; an unlisted one goes and its
            /// key is enqueued.
            #[tokio::test]
            async fn a_blob_delete_enqueues_its_key() {
                let handles = $open().await;
                let repo = handles.repository;
                let key = blob(&handles, "sha256:bb").await;
                put(&handles, "sha256:aa", &["sha256:bb".to_string()], None).await.unwrap();
                assert!(matches!(handles.oci.delete_blob(repo, "sha256:bb", at(10)).await, Err(StoreError::Conflict)));
                let manifest_key = handles.oci.manifest(repo, "app", "sha256:aa").await.unwrap().unwrap().key;
                handles.oci.delete_manifest(repo, "app", "sha256:aa", at(10)).await.unwrap();
                assert_eq!(queued(&handles).await.len(), 2);
                let loose = blob(&handles, "sha256:cc").await;
                assert!(handles.oci.delete_blob(repo, "sha256:cc", at(10)).await.unwrap());
                assert!(!handles.oci.delete_blob(repo, "sha256:cc", at(10)).await.unwrap());
                let mut want = vec![key, manifest_key, loose];
                want.sort();
                assert_eq!(queued(&handles).await, want);
            }

            /// Idle sessions without a live lease are reaped, their rows gone
            /// and their prefixes enqueued; a completing one stays.
            #[tokio::test]
            async fn idle_sessions_are_reaped_and_their_prefixes_enqueued() {
                let handles = $open().await;
                let prefix = format!("{}/_uploads/idle", handles.prefix);
                handles.oci.start_upload("idle", handles.repository, "app", &prefix, at(1)).await.unwrap();
                session(&handles, "busy").await;
                let idle = ::std::time::Duration::from_secs(1800);
                assert_eq!(handles.oci.reap_uploads(idle, at(8), 10).await.unwrap(), 1);
                assert!(handles.oci.upload("idle").await.unwrap().is_none());
                assert!(handles.oci.upload("busy").await.unwrap().is_some());
                assert_eq!(queued(&handles).await, vec![prefix]);
            }
        }
    };
}

#[allow(unused_imports)]
pub(crate) use cascade_contract;

/// What `reclaim_contract!` needs of an adapter: port 22, port 23, and the
/// ports whose rows reference keys or whose methods enqueue.
pub struct ReclaimHandles {
    pub repos: Arc<dyn RepositoryStore>,
    pub packages: Arc<dyn PackageStore>,
    pub oci: Arc<dyn OciStore>,
    pub maven: Arc<dyn opencargo::ports::maven::MavenFileStore>,
    pub reclaim: Arc<dyn opencargo::ports::reclaim::ReclaimStore>,
    pub referenced: Arc<dyn opencargo::ports::referenced::ReferencedKeys>,
    _keep: Box<dyn Any + Send>,
}

/// The referencing ports of `ReclaimHandles`.
pub struct Referencing {
    pub packages: Arc<dyn PackageStore>,
    pub oci: Arc<dyn OciStore>,
    pub maven: Arc<dyn opencargo::ports::maven::MavenFileStore>,
}

impl ReclaimHandles {
    pub fn new(
        repos: Arc<dyn RepositoryStore>,
        referencing: Referencing,
        reclaim: Arc<dyn opencargo::ports::reclaim::ReclaimStore>,
        referenced: Arc<dyn opencargo::ports::referenced::ReferencedKeys>,
        keep: Box<dyn Any + Send>,
    ) -> Self {
        Self {
            repos,
            packages: referencing.packages,
            oci: referencing.oci,
            maven: referencing.maven,
            reclaim,
            referenced,
            _keep: keep,
        }
    }
}

/// `reclaim_contract!(name, opener)`: port 22 and port 23 answer alike on
/// every adapter, and agree with each other.
#[allow(unused_macros)]
macro_rules! reclaim_contract {
    ($suite:ident, $open:path) => {
        mod $suite {
            use super::*;
            use ::chrono::{DateTime, TimeZone, Utc};
            use ::futures_util::TryStreamExt;
            use ::opencargo::domain::{layout, Format, RepoKind, RepoSpec, Repository, Visibility};
            use ::opencargo::error::StoreError;
            use ::opencargo::ports::packages::{NameMatch, NewRelease};
            use ::opencargo::ports::reclaim::{Claim, PinToken, Pinned, Renewal};
            use ::std::time::Duration;

            const GRACE: Duration = Duration::from_secs(3600);

            fn at(hour: u32) -> DateTime<Utc> {
                Utc.with_ymd_and_hms(2026, 9, 18, hour, 0, 0).unwrap()
            }

            async fn repo(h: &ReclaimHandles, name: &str) -> (Repository, String) {
                let repo = h
                    .repos
                    .create(
                        &RepoSpec {
                            name,
                            kind: RepoKind::Hosted,
                            format: Format::Npm,
                            visibility: Visibility::Public,
                            upstream: None,
                            members: &[],
                        },
                        at(1),
                    )
                    .await
                    .unwrap();
                let incarnation = h.repos.incarnation(repo.id).await.unwrap().unwrap();
                (repo, layout::incarnation_prefix(&incarnation))
            }

            async fn pin(h: &ReclaimHandles, prefix: &str, keys: &[&str], until: u32) -> Vec<PinToken> {
                let keys: Vec<String> = keys.iter().map(|k| k.to_string()).collect();
                match h.reclaim.pin(prefix, &keys, at(until)).await.unwrap() {
                    Pinned::Tokens(tokens) => tokens,
                    Pinned::Retired => panic!("{prefix} is live"),
                }
            }

            async fn publish(h: &ReclaimHandles, repo: i64, version: &str, key: &str) -> i64 {
                h.packages
                    .publish_version(&NewRelease {
                        repository: repo,
                        package: "p",
                        match_name: NameMatch::Exact,
                        description: None,
                        readme: None,
                        version,
                        metadata_json: "{}",
                        checksum_sha1: None,
                        checksum_sha256: None,
                        integrity: None,
                        size: 1,
                        tarball_path: key,
                        dist_tags: &[],
                        pins: &[],
                        now: at(2),
                    })
                    .await
                    .unwrap()
                    .version
                    .id
            }

            async fn listed(h: &ReclaimHandles, now: u32) -> Vec<String> {
                h.referenced
                    .referenced(GRACE, at(now))
                    .map_ok(|r| r.key)
                    .try_collect()
                    .await
                    .unwrap()
            }

            async fn claim(h: &ReclaimHandles, key: &str, now: u32) -> Claim {
                h.reclaim.claim(key, GRACE, at(now), at(now + 1)).await.unwrap()
            }

            #[tokio::test]
            async fn claim_revokes_every_pin_on_its_key() {
                let h = $open().await;
                let (_, prefix) = repo(&h, "r").await;
                let first = pin(&h, &prefix, &["r/x"], 2).await;
                let key = first[0].physical_key.clone();
                h.reclaim.enqueue(std::slice::from_ref(&key), at(1)).await.unwrap();
                assert_eq!(claim(&h, &key, 2).await, Claim::Pinned, "a live pin protects");
                assert!(matches!(claim(&h, &key, 5).await, Claim::Claimed(_)), "expired past the grace");
                assert!(!listed(&h, 5).await.contains(&key));
            }

            #[tokio::test]
            async fn an_expired_pin_within_the_grace_still_protects() {
                let h = $open().await;
                let (_, prefix) = repo(&h, "r").await;
                let tokens = pin(&h, &prefix, &["r/x"], 4).await;
                let key = tokens[0].physical_key.clone();
                h.reclaim.enqueue(std::slice::from_ref(&key), at(1)).await.unwrap();
                assert_eq!(claim(&h, &key, 4).await, Claim::Pinned, "expired at the instant, inside the grace");
                assert!(listed(&h, 4).await.contains(&key), "protecting pins are listed");
            }

            #[tokio::test]
            async fn claimed_generation_is_never_pinned_again() {
                let h = $open().await;
                let (r, prefix) = repo(&h, "r").await;
                let first = pin(&h, &prefix, &["r/x"], 2).await;
                let key = first[0].physical_key.clone();
                h.reclaim.enqueue(std::slice::from_ref(&key), at(1)).await.unwrap();
                assert!(matches!(claim(&h, &key, 5).await, Claim::Claimed(_)));
                publish(&h, r.id, "1.0.0", &key).await;
                let again = pin(&h, &prefix, &["r/x"], 9).await;
                assert_ne!(again[0].physical_key, key, "a claimed generation is never reused");
            }

            #[tokio::test]
            async fn pin_reuses_only_a_referenced_never_claimed_generation() {
                let h = $open().await;
                let (r, prefix) = repo(&h, "r").await;
                let fresh = pin(&h, &prefix, &["r/x"], 2).await;
                let other = pin(&h, &prefix, &["r/x"], 2).await;
                assert_ne!(fresh[0].physical_key, other[0].physical_key, "unreferenced: fresh each time");
                publish(&h, r.id, "1.0.0", &fresh[0].physical_key).await;
                let reused = pin(&h, &prefix, &["r/x"], 2).await;
                assert_eq!(reused[0].physical_key, fresh[0].physical_key);
                let batch = pin(&h, &prefix, &["r/x", "r/y"], 2).await;
                assert_eq!(batch.len(), 2, "one transaction, one token per key");
                assert!(batch[1].physical_key.starts_with("r/y~"));
            }

            #[tokio::test]
            async fn pin_under_retired_incarnation_is_refused() {
                let h = $open().await;
                let (_, prefix) = repo(&h, "r").await;
                let (_, sibling) = repo(&h, "r2").await;
                h.repos.retire("r", at(2)).await.unwrap();
                let keys = vec!["k".to_string()];
                assert_eq!(h.reclaim.pin(&prefix, &keys, at(3)).await.unwrap(), Pinned::Retired);
                assert!(matches!(
                    h.reclaim.pin(&sibling, &keys, at(3)).await.unwrap(),
                    Pinned::Tokens(_)
                ));
                let string_sibling = format!("{prefix}x");
                assert_eq!(
                    h.reclaim.pin(&string_sibling, &keys, at(3)).await.unwrap(),
                    Pinned::Retired,
                    "an unknown prefix is never live, whatever it starts with"
                );
            }

            #[tokio::test]
            async fn retire_revokes_pins_and_enqueues_its_prefixes() {
                let h = $open().await;
                let (_, prefix) = repo(&h, "r").await;
                let tokens = pin(&h, &prefix, &[&format!("{prefix}/p/f")], 9).await;
                let prefixes = h.repos.retire("r", at(2)).await.unwrap();
                assert!(prefixes.contains(&prefix));
                assert!(prefixes.contains(&"npm/r".to_string()));
                assert!(
                    !listed(&h, 2).await.contains(&tokens[0].physical_key),
                    "the pin taken before retire is revoked"
                );
                let due = h.reclaim.due(GRACE, at(2), 10).await.unwrap();
                assert!(due.iter().any(|c| c.key == prefix && c.prefix), "claimable without the grace");
                assert!(matches!(claim(&h, &prefix, 2).await, Claim::Claimed(_)));
            }

            #[tokio::test]
            async fn retire_rechecks_conflicts_in_the_transaction() {
                let h = $open().await;
                let (r, _) = repo(&h, "r").await;
                let v = publish(&h, r.id, "1.0.0", "npm/r/p/p.tgz").await;
                assert!(matches!(h.repos.retire("r", at(2)).await, Err(StoreError::Conflict)));
                assert!(h.reclaim.due(GRACE, at(9), 10).await.unwrap().is_empty(), "a refusal enqueues nothing");
                h.packages.delete_version(v, at(3)).await.unwrap();
                h.repos
                    .create(
                        &RepoSpec {
                            name: "g",
                            kind: RepoKind::Group,
                            format: Format::Npm,
                            visibility: Visibility::Public,
                            upstream: None,
                            members: &["r".to_string()],
                        },
                        at(3),
                    )
                    .await
                    .unwrap();
                assert!(matches!(h.repos.retire("r", at(4)).await, Err(StoreError::Conflict)));
            }

            #[tokio::test]
            async fn a_retired_group_enqueues_nothing() {
                let h = $open().await;
                h.repos
                    .create(
                        &RepoSpec {
                            name: "g",
                            kind: RepoKind::Group,
                            format: Format::Npm,
                            visibility: Visibility::Public,
                            upstream: None,
                            members: &[],
                        },
                        at(1),
                    )
                    .await
                    .unwrap();
                assert!(h.repos.retire("g", at(2)).await.unwrap().is_empty());
                assert!(h.reclaim.due(GRACE, at(9), 10).await.unwrap().is_empty());
            }

            #[tokio::test]
            async fn a_recreated_name_takes_its_legacy_prefix_out_of_the_queue() {
                let h = $open().await;
                repo(&h, "r").await;
                h.repos.retire("r", at(2)).await.unwrap();
                repo(&h, "r").await;
                assert_eq!(claim(&h, "npm/r", 9).await, Claim::NotDue, "the candidate went with the recreation");
                h.reclaim.enqueue_prefix("npm/r", at(3)).await.unwrap();
                assert_eq!(claim(&h, "npm/r", 9).await, Claim::Referenced, "a live incarnation's prefix is never claimed");
            }

            #[tokio::test]
            async fn recreation_refused_while_its_legacy_prefix_is_claimed() {
                let h = $open().await;
                repo(&h, "r").await;
                h.repos.retire("r", at(2)).await.unwrap();
                assert!(matches!(claim(&h, "npm/r", 2).await, Claim::Claimed(_)));
                let spec = RepoSpec {
                    name: "r",
                    kind: RepoKind::Hosted,
                    format: Format::Npm,
                    visibility: Visibility::Public,
                    upstream: None,
                    members: &[],
                };
                assert!(matches!(h.repos.create(&spec, at(2)).await, Err(StoreError::Conflict)));
            }

            #[tokio::test]
            async fn delete_version_enqueues_and_deletes_nothing() {
                let h = $open().await;
                let (r, _) = repo(&h, "r").await;
                let v = publish(&h, r.id, "1.0.0", "npm/r/p/p.tgz").await;
                assert_eq!(claim(&h, "npm/r/p/p.tgz", 9).await, Claim::NotDue, "nothing queued yet");
                h.packages.delete_version(v, at(3)).await.unwrap();
                assert!(matches!(h.packages.delete_version(v, at(3)).await, Err(StoreError::NotFound)));
                let due = h.reclaim.due(GRACE, at(5), 10).await.unwrap();
                assert_eq!(due.len(), 1);
                assert_eq!(due[0].key, "npm/r/p/p.tgz");
                assert!(matches!(claim(&h, "npm/r/p/p.tgz", 5).await, Claim::Claimed(_)));
            }

            #[tokio::test]
            async fn a_referenced_key_is_listed_and_never_claimed() {
                let h = $open().await;
                let (r, _) = repo(&h, "r").await;
                publish(&h, r.id, "1.0.0", "npm/r/p/a.tgz").await;
                h.reclaim.enqueue(&["npm/r/p/a.tgz".to_string()], at(1)).await.unwrap();
                h.reclaim.enqueue(&["npm/r/p/a.tgz".to_string()], at(1)).await.unwrap();
                assert!(listed(&h, 5).await.contains(&"npm/r/p/a.tgz".to_string()));
                assert_eq!(claim(&h, "npm/r/p/a.tgz", 5).await, Claim::Referenced);
                assert!(h.reclaim.due(GRACE, at(5), 10).await.unwrap().is_empty(), "Referenced drops the candidate");
            }

            #[tokio::test]
            async fn segments_of_a_slow_session_are_never_claimable() {
                let h = $open().await;
                let (r, prefix) = repo(&h, "r").await;
                let session = layout::upload_prefix(&prefix, "u1");
                h.oci.start_upload("u1", r.id, "app", &session, at(1)).await.unwrap();
                let segment = layout::segment_key(&session, 0, "n");
                h.reclaim.enqueue(std::slice::from_ref(&segment), at(1)).await.unwrap();
                assert_eq!(claim(&h, &segment, 20).await, Claim::Referenced);
                let refs: Vec<_> = h.referenced.referenced(GRACE, at(20)).try_collect().await.unwrap();
                assert!(refs.iter().any(|r| r.key == session && r.prefix));
            }

            #[tokio::test]
            async fn renew_after_takeover_is_superseded() {
                let h = $open().await;
                h.reclaim.enqueue(&["k".to_string()], at(1)).await.unwrap();
                let Claim::Claimed(first) = claim(&h, "k", 3).await else { panic!("due") };
                assert_eq!(claim(&h, "k", 3).await, Claim::NotDue, "a live claim is exclusive");
                let Claim::Claimed(second) = claim(&h, "k", 6).await else {
                    panic!("an expired claim is taken over")
                };
                assert_eq!(h.reclaim.renew(&first, at(6), at(7)).await.unwrap(), Renewal::Superseded);
                assert_eq!(h.reclaim.renew(&second, at(6), at(7)).await.unwrap(), Renewal::Renewed);
                h.reclaim.release(&first).await.unwrap();
                assert_eq!(claim(&h, "k", 6).await, Claim::NotDue, "a stale release is a no-op");
                h.reclaim.release(&second).await.unwrap();
                assert!(h.reclaim.due(GRACE, at(9), 10).await.unwrap().is_empty());
            }

            #[tokio::test]
            async fn crashed_placer_pins_are_pruned_past_the_grace_and_bounded() {
                let h = $open().await;
                let (_, prefix) = repo(&h, "r").await;
                pin(&h, &prefix, &["a", "b", "c"], 2).await;
                let live = pin(&h, &prefix, &["d"], 9).await;
                assert_eq!(h.reclaim.prune_pins(GRACE, at(2), 10).await.unwrap(), 0);
                assert_eq!(h.reclaim.prune_pins(GRACE, at(4), 2).await.unwrap(), 2, "bounded");
                assert_eq!(h.reclaim.prune_pins(GRACE, at(4), 10).await.unwrap(), 1);
                assert_eq!(listed(&h, 4).await, vec![live[0].physical_key.clone()]);
            }

            fn release_with<'a>(
                repo: i64,
                version: &'a str,
                pins: &'a [PinToken],
            ) -> NewRelease<'a> {
                NewRelease {
                    repository: repo,
                    package: "p",
                    match_name: NameMatch::Exact,
                    description: None,
                    readme: None,
                    version,
                    metadata_json: "{}",
                    checksum_sha1: None,
                    checksum_sha256: None,
                    integrity: None,
                    size: 1,
                    tarball_path: &pins[0].physical_key,
                    dist_tags: &[],
                    pins,
                    now: at(2),
                }
            }

            #[tokio::test]
            async fn commit_with_revoked_pin_is_superseded() {
                use ::opencargo::ports::packages::{Promotion, PromotionAudit};
                let h = $open().await;
                let (r, prefix) = repo(&h, "r").await;
                let tokens = pin(&h, &prefix, &["r/x"], 2).await;
                h.reclaim
                    .enqueue(std::slice::from_ref(&tokens[0].physical_key), at(1))
                    .await
                    .unwrap();
                assert!(matches!(claim(&h, &tokens[0].physical_key, 5).await, Claim::Claimed(_)));
                let refused = h
                    .packages
                    .publish_version(&release_with(r.id, "1.0.0", &tokens))
                    .await;
                match &refused {
                    Err(StoreError::Superseded(keys)) => {
                        assert_eq!(keys, &vec![tokens[0].physical_key.clone()])
                    }
                    other => panic!("{other:?}"),
                }
                assert!(
                    h.packages.package(r.id, "p", NameMatch::Exact).await.unwrap().is_none(),
                    "nothing written"
                );

                let live = pin(&h, &prefix, &["r/y"], 9).await;
                let landed = h
                    .packages
                    .publish_version(&release_with(r.id, "1.0.0", &live))
                    .await
                    .unwrap();
                let (target, target_prefix) = repo(&h, "t").await;
                let revoked = pin(&h, &target_prefix, &["t/x"], 2).await;
                h.reclaim
                    .enqueue(std::slice::from_ref(&revoked[0].physical_key), at(1))
                    .await
                    .unwrap();
                assert!(matches!(claim(&h, &revoked[0].physical_key, 5).await, Claim::Claimed(_)));
                let promoted = h
                    .packages
                    .promote_metadata(&Promotion {
                        source: &landed.version,
                        target_repository: target.id,
                        package: "p",
                        description: None,
                        metadata_json: "{}",
                        tarball_path: &revoked[0].physical_key,
                        dist_tags: &[],
                        pins: &revoked,
                        audit: PromotionAudit {
                            user_id: None,
                            username: "u",
                            target: "p@1.0.0",
                            repository: "t",
                            details_json: "{}",
                        },
                        now: at(6),
                    })
                    .await;
                assert!(matches!(promoted, Err(StoreError::Superseded(_))), "{promoted:?}");
                assert!(h
                    .packages
                    .package(target.id, "p", NameMatch::Exact)
                    .await
                    .unwrap()
                    .is_none());
            }

            #[tokio::test]
            async fn a_spent_pin_is_gone_and_a_row_references_its_key() {
                let h = $open().await;
                let (r, prefix) = repo(&h, "r").await;
                let tokens = pin(&h, &prefix, &["r/x"], 9).await;
                h.packages
                    .publish_version(&release_with(r.id, "1.0.0", &tokens))
                    .await
                    .unwrap();
                let again = h
                    .packages
                    .publish_version(&release_with(r.id, "1.0.1", &tokens))
                    .await;
                assert!(matches!(again, Err(StoreError::Superseded(_))), "a token is spent once");
                h.reclaim
                    .enqueue(std::slice::from_ref(&tokens[0].physical_key), at(1))
                    .await
                    .unwrap();
                assert_eq!(claim(&h, &tokens[0].physical_key, 5).await, Claim::Referenced);
            }

            #[tokio::test]
            async fn commit_after_retire_is_superseded_not_fk_error() {
                let h = $open().await;
                let (r, prefix) = repo(&h, "r").await;
                let tokens = pin(&h, &prefix, &[&format!("{prefix}/p/f")], 9).await;
                h.repos.retire("r", at(2)).await.unwrap();
                let refused = h
                    .packages
                    .publish_version(&release_with(r.id, "1.0.0", &tokens))
                    .await;
                assert!(matches!(refused, Err(StoreError::Superseded(_))), "{refused:?}");
            }

            #[tokio::test]
            async fn maven_keys_agree_between_ports_18_and_23() {
                use ::opencargo::ports::maven::{Digests, NewFile, UnitChange, UnitKey};
                let h = $open().await;
                let (r, prefix) = repo(&h, "r").await;
                let tokens = pin(&h, &prefix, &[&format!("{prefix}/g/a/x/a-1.jar")], 2).await;
                let key = UnitKey { repository: r.id, ga: "g:a", version: "1", build: "" };
                let digests = Digests::default();
                h.maven
                    .change(&UnitChange {
                        key,
                        revision: None,
                        depositor: "alice",
                        file: Some(NewFile {
                            filename: "a-1.jar",
                            physical_key: &tokens[0].physical_key,
                            size: 1,
                            digests: &digests,
                            depositor: "alice",
                        }),
                        declarations: &[],
                        contest: false,
                        reveal: false,
                        scopes: &[],
                        pins: &tokens,
                        now: at(2),
                    })
                    .await
                    .unwrap();
                let physical = tokens[0].physical_key.clone();
                assert!(listed(&h, 10).await.contains(&physical));
                h.reclaim.enqueue(std::slice::from_ref(&physical), at(1)).await.unwrap();
                assert_eq!(claim(&h, &physical, 10).await, Claim::Referenced);

                let released = h.maven.refuse(&key, 1, &[], at(11)).await.unwrap().released;
                assert_eq!(released, vec![physical.clone()]);
                assert!(!listed(&h, 13).await.contains(&physical));
                assert!(matches!(claim(&h, &physical, 13).await, Claim::Claimed(_)));
            }

            #[tokio::test]
            async fn forgetting_a_retired_prefix_keeps_the_rest_retired() {
                let h = $open().await;
                let (_, prefix) = repo(&h, "r").await;
                h.repos.retire("r", at(2)).await.unwrap();
                h.reclaim.forget_retired("npm/r").await.unwrap();
                let keys = vec!["k".to_string()];
                assert_eq!(h.reclaim.pin(&prefix, &keys, at(3)).await.unwrap(), Pinned::Retired);
            }

            #[tokio::test]
            async fn backlog_counts_candidates_and_prefixes() {
                let h = $open().await;
                assert_eq!(h.reclaim.backlog().await.unwrap(), ::opencargo::ports::reclaim::Backlog::default());
                h.reclaim.enqueue(&["a".to_string(), "b".to_string(), "a".to_string()], at(1)).await.unwrap();
                h.reclaim.enqueue_prefix("p", at(1)).await.unwrap();
                let backlog = h.reclaim.backlog().await.unwrap();
                assert_eq!((backlog.candidates, backlog.prefixes), (3, 1));
            }
        }
    };
}

#[allow(unused_imports)]
pub(crate) use reclaim_contract;

/// What `pypi_contract!` needs: port 15 and the ports its rows answer to.
pub struct PypiHandles {
    pub repos: Arc<dyn RepositoryStore>,
    pub pypi: Arc<dyn opencargo::ports::pypi::PypiFileStore>,
    pub reclaim: Arc<dyn opencargo::ports::reclaim::ReclaimStore>,
    pub referenced: Arc<dyn opencargo::ports::referenced::ReferencedKeys>,
    _keep: Box<dyn Any + Send>,
}

impl PypiHandles {
    pub fn new(
        repos: Arc<dyn RepositoryStore>,
        pypi: Arc<dyn opencargo::ports::pypi::PypiFileStore>,
        reclaim: Arc<dyn opencargo::ports::reclaim::ReclaimStore>,
        referenced: Arc<dyn opencargo::ports::referenced::ReferencedKeys>,
        keep: Box<dyn Any + Send>,
    ) -> Self {
        Self {
            repos,
            pypi,
            reclaim,
            referenced,
            _keep: keep,
        }
    }
}

/// `pypi_contract!(name, opener)`: port 15 answers alike on every adapter,
/// and the keys its rows reference are the ones port 22 refuses to claim and
/// port 23 lists.
#[allow(unused_macros)]
macro_rules! pypi_contract {
    ($suite:ident, $open:path) => {
        mod $suite {
            use super::*;
            use ::chrono::{DateTime, TimeZone, Utc};
            use ::futures_util::TryStreamExt;
            use ::opencargo::domain::{layout, Format, RepoKind, RepoSpec, Repository, Visibility};
            use ::opencargo::error::StoreError;
            use ::opencargo::ports::pypi::{NewPypiFile, Published};
            use ::opencargo::ports::reclaim::{Claim, PinToken, Pinned};
            use ::std::time::Duration;

            const GRACE: Duration = Duration::from_secs(3600);

            fn at(hour: u32) -> DateTime<Utc> {
                Utc.with_ymd_and_hms(2026, 9, 18, hour, 0, 0).unwrap()
            }

            async fn repo(h: &PypiHandles, name: &str) -> (Repository, String) {
                let repo = h
                    .repos
                    .create(
                        &RepoSpec {
                            name,
                            kind: RepoKind::Hosted,
                            format: Format::Pypi,
                            visibility: Visibility::Public,
                            upstream: None,
                            members: &[],
                        },
                        at(1),
                    )
                    .await
                    .unwrap();
                let incarnation = h.repos.incarnation(repo.id).await.unwrap().unwrap();
                (repo, layout::incarnation_prefix(&incarnation))
            }

            async fn pins(h: &PypiHandles, prefix: &str, filename: &str, metadata: bool) -> Vec<PinToken> {
                let mut keys = vec![format!("{prefix}/demo/ab/{filename}")];
                if metadata {
                    keys.push(format!("{prefix}/demo/cd/{filename}.metadata"));
                }
                match h.reclaim.pin(prefix, &keys, at(9)).await.unwrap() {
                    Pinned::Tokens(tokens) => tokens,
                    Pinned::Retired => panic!("{prefix} is live"),
                }
            }

            fn file<'a>(repo: i64, version: &'a str, filename: &'a str, pins: &'a [PinToken]) -> NewPypiFile<'a> {
                NewPypiFile {
                    repository: repo,
                    project: "demo",
                    summary: Some("a demo"),
                    version,
                    metadata_json: "{}",
                    filename,
                    packagetype: "bdist_wheel",
                    sha256: "ab",
                    size: 3,
                    metadata_sha256: (pins.len() > 1).then_some("cd"),
                    requires_python: Some(">=3.8"),
                    pins,
                    now: at(2),
                }
            }

            async fn publish(h: &PypiHandles, repo: i64, prefix: &str, version: &str, filename: &str) -> Published {
                let tokens = pins(h, prefix, filename, true).await;
                h.pypi.publish_file(&file(repo, version, filename, &tokens)).await.unwrap()
            }

            async fn listed(h: &PypiHandles) -> Vec<String> {
                h.referenced
                    .referenced(GRACE, at(5))
                    .map_ok(|r| r.key)
                    .try_collect()
                    .await
                    .unwrap()
            }

            #[tokio::test]
            async fn a_file_lands_its_release_and_records_the_keys_it_was_given() {
                let h = $open().await;
                let (r, prefix) = repo(&h, "py").await;
                let tokens = pins(&h, &prefix, "demo-1.0-py3-none-any.whl", true).await;
                let first = h
                    .pypi
                    .publish_file(&file(r.id, "1", "demo-1.0-py3-none-any.whl", &tokens))
                    .await
                    .unwrap();
                assert!(first.version_created);
                assert_eq!(first.file.key, tokens[0].physical_key);
                assert_eq!(first.file.metadata_key.as_deref(), Some(tokens[1].physical_key.as_str()));
                assert_eq!(first.file.uploaded_at, at(2), "the caller's clock, read back as given");
                assert_eq!((first.file.project.as_str(), first.file.version.as_str()), ("demo", "1"));
                let second = publish(&h, r.id, &prefix, "1", "demo-1.0.tar.gz").await;
                assert!(!second.version_created, "one release, two files");
                assert_eq!(second.file.version_id, first.file.version_id);
                let files = h.pypi.project_files(r.id, "demo").await.unwrap();
                assert_eq!(
                    files.iter().map(|f| f.filename.as_str()).collect::<Vec<_>>(),
                    ["demo-1.0-py3-none-any.whl", "demo-1.0.tar.gz"]
                );
                assert_eq!(h.pypi.list_projects(r.id).await.unwrap(), ["demo"]);
                let found = h.pypi.file_by_name(r.id, "demo-1.0.tar.gz").await.unwrap().unwrap();
                assert_eq!(found, second.file);
                assert!(h.pypi.file_by_name(r.id, "nope-1.0.tar.gz").await.unwrap().is_none());
            }

            #[tokio::test]
            async fn a_second_file_of_one_name_is_a_conflict_that_writes_nothing() {
                let h = $open().await;
                let (r, prefix) = repo(&h, "py").await;
                publish(&h, r.id, &prefix, "1", "demo-1.0.tar.gz").await;
                let tokens = pins(&h, &prefix, "demo-1.0.tar.gz", false).await;
                let refused = h.pypi.publish_file(&file(r.id, "2", "demo-1.0.tar.gz", &tokens)).await;
                assert!(matches!(refused, Err(StoreError::Conflict)), "{refused:?}");
                let files = h.pypi.project_files(r.id, "demo").await.unwrap();
                assert_eq!(files.len(), 1);
                assert_eq!(files[0].version, "1", "the refused file created no release");
            }

            #[tokio::test]
            async fn two_concurrent_publishes_of_one_filename_yield_one_conflict() {
                let h = $open().await;
                let (r, prefix) = repo(&h, "py").await;
                let a = pins(&h, &prefix, "demo-1.0.tar.gz", false).await;
                let b = pins(&h, &prefix, "demo-1.0.tar.gz", false).await;
                let (fa, fb) = (file(r.id, "1", "demo-1.0.tar.gz", &a), file(r.id, "1", "demo-1.0.tar.gz", &b));
                let (x, y) = ::tokio::join!(h.pypi.publish_file(&fa), h.pypi.publish_file(&fb));
                let outcomes = [x, y];
                assert_eq!(outcomes.iter().filter(|o| o.is_ok()).count(), 1);
                assert_eq!(
                    outcomes.iter().filter(|o| matches!(o, Err(StoreError::Conflict))).count(),
                    1,
                    "{outcomes:?}"
                );
            }

            #[tokio::test]
            async fn a_revoked_pin_is_superseded_and_writes_nothing() {
                let h = $open().await;
                let (r, prefix) = repo(&h, "py").await;
                let keys = [format!("{prefix}/demo/ab/w.whl"), format!("{prefix}/demo/cd/w.whl.metadata")];
                let Pinned::Tokens(tokens) = h.reclaim.pin(&prefix, &keys, at(2)).await.unwrap() else {
                    panic!("{prefix} is live")
                };
                h.reclaim.enqueue(std::slice::from_ref(&tokens[1].physical_key), at(1)).await.unwrap();
                assert!(matches!(
                    h.reclaim.claim(&tokens[1].physical_key, GRACE, at(5), at(6)).await.unwrap(),
                    Claim::Claimed(_)
                ));
                let refused = h
                    .pypi
                    .publish_file(&file(r.id, "1", "demo-1.0-py3-none-any.whl", &tokens))
                    .await;
                match refused {
                    Err(StoreError::Superseded(keys)) => assert_eq!(keys, vec![tokens[1].physical_key.clone()]),
                    other => panic!("{other:?}"),
                }
                assert!(h.pypi.project_files(r.id, "demo").await.unwrap().is_empty());
                assert!(h.pypi.list_projects(r.id).await.unwrap().is_empty());
            }

            #[tokio::test]
            async fn a_yank_moves_the_release_and_every_file_together() {
                let h = $open().await;
                let (r, prefix) = repo(&h, "py").await;
                publish(&h, r.id, &prefix, "1", "demo-1.0.tar.gz").await;
                publish(&h, r.id, &prefix, "1", "demo-1.0-py3-none-any.whl").await;
                publish(&h, r.id, &prefix, "2", "demo-2.0.tar.gz").await;
                h.pypi.set_release_yanked(r.id, "demo", "1", Some("broken"), true, at(3)).await.unwrap();
                let files = h.pypi.project_files(r.id, "demo").await.unwrap();
                for f in &files {
                    let yanked = f.version == "1";
                    assert_eq!(f.yanked, yanked, "{}", f.filename);
                    assert_eq!(f.yanked_reason.as_deref(), yanked.then_some("broken"));
                }
                h.pypi.set_release_yanked(r.id, "demo", "1", None, false, at(4)).await.unwrap();
                assert!(h
                    .pypi
                    .project_files(r.id, "demo")
                    .await
                    .unwrap()
                    .iter()
                    .all(|f| !f.yanked && f.yanked_reason.is_none()));
                assert!(matches!(
                    h.pypi.set_release_yanked(r.id, "demo", "9", None, true, at(4)).await,
                    Err(StoreError::NotFound)
                ));
            }

            #[tokio::test]
            async fn a_key_this_port_references_metadata_included_is_referenced_for_claim_and_listed() {
                let h = $open().await;
                let (r, prefix) = repo(&h, "py").await;
                let landed = publish(&h, r.id, &prefix, "1", "demo-1.0-py3-none-any.whl").await.file;
                let metadata = landed.metadata_key.clone().unwrap();
                let listing = listed(&h).await;
                for key in [&landed.key, &metadata] {
                    assert!(listing.contains(key), "{key} listed");
                    h.reclaim.enqueue(std::slice::from_ref(key), at(1)).await.unwrap();
                    assert_eq!(
                        h.reclaim.claim(key, GRACE, at(5), at(6)).await.unwrap(),
                        Claim::Referenced,
                        "{key}"
                    );
                }
            }

            #[tokio::test]
            async fn deleting_a_release_enqueues_every_key_it_held_and_deletes_nothing() {
                let h = $open().await;
                let (r, prefix) = repo(&h, "py").await;
                let wheel = publish(&h, r.id, &prefix, "1", "demo-1.0-py3-none-any.whl").await.file;
                let sdist = publish(&h, r.id, &prefix, "1", "demo-1.0.tar.gz").await.file;
                let kept = publish(&h, r.id, &prefix, "2", "demo-2.0.tar.gz").await.file;
                let mut want = vec![
                    wheel.key.clone(),
                    wheel.metadata_key.clone().unwrap(),
                    sdist.key.clone(),
                    sdist.metadata_key.clone().unwrap(),
                ];
                want.sort();
                let released = h.pypi.delete_release(r.id, "demo", "1", at(3)).await.unwrap();
                assert_eq!(released, want);
                assert!(matches!(
                    h.pypi.delete_release(r.id, "demo", "1", at(3)).await,
                    Err(StoreError::NotFound)
                ));
                let mut due: Vec<String> =
                    h.reclaim.due(GRACE, at(5), 50).await.unwrap().into_iter().map(|c| c.key).collect();
                due.sort();
                assert_eq!(due, want, "enqueued in the delete's own transaction");
                for key in &want {
                    assert!(
                        matches!(h.reclaim.claim(key, GRACE, at(5), at(6)).await.unwrap(), Claim::Claimed(_)),
                        "{key}"
                    );
                }
                assert_eq!(h.pypi.project_files(r.id, "demo").await.unwrap(), vec![kept.clone()]);
                let rest = h.pypi.delete_project_files(r.id, "demo", at(3)).await.unwrap();
                assert!(rest.contains(&kept.key) && rest.contains(kept.metadata_key.as_ref().unwrap()));
                assert!(h.pypi.list_projects(r.id).await.unwrap().is_empty());
            }

            #[tokio::test]
            async fn a_repository_holding_files_refuses_to_retire() {
                let h = $open().await;
                let (r, prefix) = repo(&h, "py").await;
                publish(&h, r.id, &prefix, "1", "demo-1.0.tar.gz").await;
                assert!(matches!(h.repos.retire("py", at(3)).await, Err(StoreError::Conflict)));
                assert_eq!(h.pypi.list_projects(r.id).await.unwrap(), ["demo"]);
            }
        }
    };
}

#[allow(unused_imports)]
pub(crate) use pypi_contract;

/// What `maven_contract!` needs of an adapter: port 18, and the ports it
/// fences with and contributes to.
pub struct MavenHandles {
    pub repos: Arc<dyn RepositoryStore>,
    pub maven: Arc<dyn opencargo::ports::maven::MavenFileStore>,
    pub reclaim: Arc<dyn opencargo::ports::reclaim::ReclaimStore>,
    pub referenced: Arc<dyn opencargo::ports::referenced::ReferencedKeys>,
    _keep: Box<dyn Any + Send>,
}

impl MavenHandles {
    pub fn new(
        repos: Arc<dyn RepositoryStore>,
        maven: Arc<dyn opencargo::ports::maven::MavenFileStore>,
        reclaim: Arc<dyn opencargo::ports::reclaim::ReclaimStore>,
        referenced: Arc<dyn opencargo::ports::referenced::ReferencedKeys>,
        keep: Box<dyn Any + Send>,
    ) -> Self {
        Self {
            repos,
            maven,
            reclaim,
            referenced,
            _keep: keep,
        }
    }
}

/// `maven_contract!(name, opener)`: port 18 answers alike on every adapter.
#[allow(unused_macros)]
macro_rules! maven_contract {
    ($suite:ident, $open:path) => {
        mod $suite {
            use super::*;
            use ::chrono::{DateTime, TimeZone, Utc};
            use ::futures_util::TryStreamExt;
            use ::opencargo::domain::{layout, Format, RepoKind, RepoSpec, Visibility};
            use ::opencargo::error::StoreError;
            use ::opencargo::ports::maven::{
                ClientMetadata, Declaration, Digests, NewFile, SumAlgorithm, UnitChange, UnitKey,
            };
            use ::opencargo::ports::reclaim::{Claim, PinToken, Pinned};
            use ::std::time::Duration;

            const GRACE: Duration = Duration::from_secs(3600);
            const GA: &str = "org.example:lib";

            fn at(hour: u32) -> DateTime<Utc> {
                Utc.with_ymd_and_hms(2026, 9, 18, hour, 0, 0).unwrap()
            }

            fn digests(seed: &str) -> Digests {
                Digests {
                    sha1: format!("{seed}1"),
                    md5: format!("{seed}5"),
                    sha256: format!("{seed}256"),
                    sha512: format!("{seed}512"),
                }
            }

            async fn repo(h: &MavenHandles) -> (i64, String) {
                let repo = h
                    .repos
                    .create(
                        &RepoSpec {
                            name: "m",
                            kind: RepoKind::Hosted,
                            format: Format::Maven,
                            visibility: Visibility::Public,
                            upstream: None,
                            members: &[],
                        },
                        at(1),
                    )
                    .await
                    .unwrap();
                let incarnation = h.repos.incarnation(repo.id).await.unwrap().unwrap();
                (repo.id, layout::incarnation_prefix(&incarnation))
            }

            async fn pin(h: &MavenHandles, prefix: &str, logical: &str, until: u32) -> Vec<PinToken> {
                match h.reclaim.pin(prefix, &[logical.to_string()], at(until)).await.unwrap() {
                    Pinned::Tokens(tokens) => tokens,
                    Pinned::Retired => panic!("{prefix} is live"),
                }
            }

            fn key(repository: i64, version: &str) -> UnitKey<'_> {
                UnitKey {
                    repository,
                    ga: GA,
                    version,
                    build: "",
                }
            }

            fn scopes() -> Vec<String> {
                vec![GA.to_string()]
            }

            struct Deposit<'a> {
                repository: i64,
                version: &'a str,
                revision: Option<i64>,
                filename: &'a str,
                pins: &'a [PinToken],
                digests: &'a Digests,
                declarations: &'a [Declaration],
                contest: bool,
                reveal: bool,
                scopes: &'a [String],
                now: u32,
            }

            async fn deposit(h: &MavenHandles, d: Deposit<'_>) -> Result<::opencargo::ports::maven::Changed, StoreError> {
                h.maven
                    .change(&UnitChange {
                        key: key(d.repository, d.version),
                        revision: d.revision,
                        depositor: "alice",
                        file: d.pins.first().map(|p| NewFile {
                            filename: d.filename,
                            physical_key: &p.physical_key,
                            size: 3,
                            digests: d.digests,
                            depositor: "alice",
                        }),
                        declarations: d.declarations,
                        contest: d.contest,
                        reveal: d.reveal,
                        scopes: d.scopes,
                        pins: d.pins,
                        now: at(d.now),
                    })
                    .await
            }

            fn plain<'a>(
                repository: i64,
                revision: Option<i64>,
                pins: &'a [PinToken],
                digests: &'a Digests,
                scopes: &'a [String],
            ) -> Deposit<'a> {
                Deposit {
                    repository,
                    version: "1.0",
                    revision,
                    filename: "lib-1.0.jar",
                    pins,
                    digests,
                    declarations: &[],
                    contest: false,
                    reveal: false,
                    scopes,
                    now: 2,
                }
            }

            async fn due(h: &MavenHandles) -> Vec<String> {
                h.reclaim
                    .due(Duration::ZERO, at(20), 100)
                    .await
                    .unwrap()
                    .into_iter()
                    .map(|c| c.key)
                    .collect()
            }

            #[tokio::test]
            async fn a_unit_round_trips_with_its_files_declarations_and_depositor() {
                let h = $open().await;
                let (r, prefix) = repo(&h).await;
                let pins = pin(&h, &prefix, &format!("{prefix}/a"), 9).await;
                let d = digests("a");
                let declared = [Declaration {
                    filename: "lib-1.0.pom".to_string(),
                    algorithm: SumAlgorithm::Sha1,
                    value: "ff".to_string(),
                }];
                let s = scopes();
                let changed = deposit(&h, Deposit { declarations: &declared, ..plain(r, None, &pins, &d, &s) })
                    .await
                    .unwrap();
                assert_eq!(changed.revision, 1);
                assert!(changed.released.is_empty());
                let unit = h.maven.unit(&key(r, "1.0")).await.unwrap().unwrap();
                assert_eq!(unit.revision, 1);
                assert_eq!(unit.depositor, "alice");
                assert!(!unit.contested && !unit.refused && unit.visible_at.is_none());
                assert_eq!(unit.created_at, at(2));
                let file = unit.file("lib-1.0.jar").unwrap();
                assert_eq!(file.physical_key, pins[0].physical_key);
                assert_eq!(file.digests, d);
                assert_eq!(file.size, 3);
                assert_eq!(unit.declarations, declared.to_vec());
                assert!(h.maven.unit(&key(r, "2.0")).await.unwrap().is_none());
            }

            #[tokio::test]
            async fn replacing_a_file_releases_its_key_and_its_declarations() {
                let h = $open().await;
                let (r, prefix) = repo(&h).await;
                let first = pin(&h, &prefix, &format!("{prefix}/a"), 9).await;
                let (d1, d2) = (digests("a"), digests("b"));
                let s = scopes();
                let declared = [Declaration {
                    filename: "lib-1.0.jar".to_string(),
                    algorithm: SumAlgorithm::Md5,
                    value: "a5".to_string(),
                }];
                deposit(&h, Deposit { declarations: &declared, ..plain(r, None, &first, &d1, &s) })
                    .await
                    .unwrap();
                let second = pin(&h, &prefix, &format!("{prefix}/b"), 9).await;
                let changed = deposit(&h, plain(r, Some(1), &second, &d2, &s)).await.unwrap();
                assert_eq!(changed.released, vec![first[0].physical_key.clone()]);
                assert!(due(&h).await.contains(&first[0].physical_key), "enqueued, not deleted");
                let unit = h.maven.unit(&key(r, "1.0")).await.unwrap().unwrap();
                assert_eq!(unit.files.len(), 1);
                assert_eq!(unit.files[0].physical_key, second[0].physical_key);
                assert!(unit.declarations.is_empty(), "the old bytes' declarations went with them");
            }

            #[tokio::test]
            async fn a_revoked_pin_is_superseded_and_writes_nothing() {
                let h = $open().await;
                let (r, prefix) = repo(&h).await;
                let pins = pin(&h, &prefix, &format!("{prefix}/a"), 2).await;
                h.reclaim.enqueue(&[pins[0].physical_key.clone()], at(1)).await.unwrap();
                let claim = h.reclaim.claim(&pins[0].physical_key, GRACE, at(5), at(6)).await.unwrap();
                assert!(matches!(claim, Claim::Claimed(_)), "{claim:?}");
                let d = digests("a");
                let s = scopes();
                let refused = deposit(&h, plain(r, None, &pins, &d, &s)).await;
                assert!(
                    matches!(&refused, Err(StoreError::Superseded(keys)) if keys == &vec![pins[0].physical_key.clone()]),
                    "{refused:?}"
                );
                assert!(h.maven.unit(&key(r, "1.0")).await.unwrap().is_none());
                assert_eq!(h.maven.counter(r, GA).await.unwrap().value, 0);
            }

            #[tokio::test]
            async fn a_referenced_key_is_referenced_for_claim_and_listed() {
                let h = $open().await;
                let (r, prefix) = repo(&h).await;
                let pins = pin(&h, &prefix, &format!("{prefix}/a"), 2).await;
                let d = digests("a");
                let s = scopes();
                deposit(&h, plain(r, None, &pins, &d, &s)).await.unwrap();
                let physical = pins[0].physical_key.clone();
                let listed: Vec<String> = h
                    .referenced
                    .referenced(GRACE, at(10))
                    .map_ok(|k| k.key)
                    .try_collect()
                    .await
                    .unwrap();
                assert!(listed.contains(&physical), "{listed:?}");
                h.reclaim.enqueue(std::slice::from_ref(&physical), at(1)).await.unwrap();
                assert_eq!(
                    h.reclaim.claim(&physical, GRACE, at(10), at(11)).await.unwrap(),
                    Claim::Referenced
                );
            }

            #[tokio::test]
            async fn a_stale_revision_or_a_second_creation_is_a_conflict() {
                let h = $open().await;
                let (r, _) = repo(&h).await;
                let d = digests("a");
                let s = scopes();
                deposit(&h, plain(r, None, &[], &d, &s)).await.unwrap();
                let again = deposit(&h, plain(r, None, &[], &d, &s)).await;
                assert!(matches!(again, Err(StoreError::Conflict)), "{again:?}");
                deposit(&h, plain(r, Some(1), &[], &d, &s)).await.unwrap();
                let stale = deposit(&h, plain(r, Some(1), &[], &d, &s)).await;
                assert!(matches!(stale, Err(StoreError::Conflict)), "{stale:?}");

                let fresh = |v: &'static str| {
                    let h = &h;
                    let d = &d;
                    let s = &s;
                    async move {
                        deposit(h, Deposit { version: v, ..plain(r, None, &[], d, s) }).await
                    }
                };
                let (a, b) = tokio::join!(fresh("2.0"), fresh("2.0"));
                let conflicts = [&a, &b].iter().filter(|x| matches!(x, Err(StoreError::Conflict))).count();
                let oks = [&a, &b].iter().filter(|x| x.is_ok()).count();
                assert_eq!((oks, conflicts), (1, 1), "{a:?} {b:?}");
            }

            #[tokio::test]
            async fn the_contest_and_visibility_marks_stick() {
                let h = $open().await;
                let (r, _) = repo(&h).await;
                let d = digests("a");
                let s = scopes();
                deposit(&h, Deposit { contest: true, ..plain(r, None, &[], &d, &s) }).await.unwrap();
                deposit(&h, Deposit { reveal: true, now: 3, ..plain(r, Some(1), &[], &d, &s) })
                    .await
                    .unwrap();
                deposit(&h, Deposit { reveal: true, now: 4, ..plain(r, Some(2), &[], &d, &s) })
                    .await
                    .unwrap();
                let unit = h.maven.unit(&key(r, "1.0")).await.unwrap().unwrap();
                assert!(unit.contested, "nothing clears the mark");
                assert_eq!(unit.visible_at, Some(at(3)), "revealed once");
                assert!(unit.visible());
            }

            #[tokio::test]
            async fn every_change_moves_the_counters_it_names_and_no_other() {
                let h = $open().await;
                let (r, _) = repo(&h).await;
                let d = digests("a");
                let both = vec![GA.to_string(), format!("{GA}@1.0")];
                deposit(&h, plain(r, None, &[], &d, &both)).await.unwrap();
                let one = scopes();
                deposit(&h, Deposit { now: 5, ..plain(r, Some(1), &[], &d, &one) }).await.unwrap();
                let ga = h.maven.counter(r, GA).await.unwrap();
                assert_eq!((ga.value, ga.updated_at), (2, Some(at(5))));
                assert_eq!(h.maven.counter(r, &format!("{GA}@1.0")).await.unwrap().value, 1);
                assert_eq!(h.maven.counter(r, "other").await.unwrap().value, 0);
                h.maven.refuse(&key(r, "1.0"), 2, &one, at(6)).await.unwrap();
                assert_eq!(h.maven.counter(r, GA).await.unwrap().value, 3);
            }

            #[tokio::test]
            async fn refusing_releases_every_key_and_keeps_the_unit_refused() {
                let h = $open().await;
                let (r, prefix) = repo(&h).await;
                let pins = pin(&h, &prefix, &format!("{prefix}/a"), 9).await;
                let d = digests("a");
                let s = scopes();
                deposit(&h, plain(r, None, &pins, &d, &s)).await.unwrap();
                let stale = h.maven.refuse(&key(r, "1.0"), 7, &s, at(3)).await;
                assert!(matches!(stale, Err(StoreError::Conflict)), "{stale:?}");
                let changed = h.maven.refuse(&key(r, "1.0"), 1, &s, at(3)).await.unwrap();
                assert_eq!(changed.released, vec![pins[0].physical_key.clone()]);
                assert!(due(&h).await.contains(&pins[0].physical_key));
                let unit = h.maven.unit(&key(r, "1.0")).await.unwrap().unwrap();
                assert!(unit.refused && unit.files.is_empty() && !unit.visible());
                let views = h.maven.artifact(r, GA).await.unwrap();
                assert!(views.iter().all(|v| v.refused));
            }

            #[tokio::test]
            async fn a_repository_holding_maven_values_is_not_retired() {
                let h = $open().await;
                let (r, _) = repo(&h).await;
                let d = digests("a");
                let s = scopes();
                deposit(&h, plain(r, None, &[], &d, &s)).await.unwrap();
                let refused = h.repos.retire("m", at(3)).await;
                assert!(matches!(refused, Err(StoreError::Conflict)), "{refused:?}");
                assert!(h.repos.by_name("m").await.unwrap().is_some());
            }

            #[tokio::test]
            async fn pending_and_unversioned_list_what_the_reconciler_needs() {
                let h = $open().await;
                let (r, prefix) = repo(&h).await;
                let d = digests("a");
                let s = scopes();
                let waiting = [Declaration {
                    filename: "lib-sources.jar".to_string(),
                    algorithm: SumAlgorithm::Sha1,
                    value: d.sha1.clone(),
                }];
                for (i, version) in ["0.1", "0.2", "0.3", "1.0"].into_iter().enumerate() {
                    let pins = pin(&h, &prefix, &format!("{prefix}/{i}"), 9).await;
                    let files: &[PinToken] = if version == "0.3" { &[] } else { &pins };
                    let declarations: &[Declaration] = if version == "0.2" { &waiting } else { &[] };
                    deposit(&h, Deposit { version, contest: version == "0.1", declarations, ..plain(r, None, files, &d, &s) })
                        .await
                        .unwrap();
                }
                deposit(&h, Deposit { version: "2.0", now: 4, reveal: true, ..plain(r, None, &[], &d, &s) })
                    .await
                    .unwrap();
                deposit(&h, Deposit { version: "3.0", now: 4, reveal: true, ..plain(r, None, &[], &d, &s) })
                    .await
                    .unwrap();
                let pending = h.maven.pending(at(3), 1).await.unwrap();
                assert_eq!(pending.len(), 1, "contested, waiting and empty units are not offered");
                assert_eq!((pending[0].ga.as_str(), pending[0].version.as_str()), (GA, "1.0"));
                assert!(h.maven.pending(at(2), 10).await.unwrap().is_empty(), "created at 2, not before");
                let first = h.maven.unversioned(None, 1).await.unwrap();
                assert_eq!(first.len(), 1);
                assert_eq!(first[0].version, "2.0");
                let next = h.maven.unversioned(Some(&first[0]), 10).await.unwrap();
                assert_eq!(next.len(), 1);
                assert_eq!(next[0].version, "3.0", "paged past the cursor");
                h.maven.mark_versioned(r, GA, "2.0").await.unwrap();
                h.maven.mark_versioned(r, GA, "3.0").await.unwrap();
                assert!(h.maven.unversioned(None, 10).await.unwrap().is_empty());
            }

            #[tokio::test]
            async fn a_client_document_round_trips_and_moves_its_counters() {
                let h = $open().await;
                let (r, _) = repo(&h).await;
                let doc = ClientMetadata {
                    repository: r,
                    dir: "org/example/lib".to_string(),
                    digests: digests("m"),
                    release: Some("1.0".to_string()),
                    latest: None,
                    plugins: vec![("ex".to_string(), "ex-maven-plugin".to_string(), "Ex".to_string())],
                };
                let s = scopes();
                h.maven.record_client_metadata(&doc, &s, at(3)).await.unwrap();
                assert_eq!(h.maven.client_metadata(r, "org/example/lib").await.unwrap(), Some(doc.clone()));
                let newer = ClientMetadata { release: None, ..doc };
                h.maven.record_client_metadata(&newer, &s, at(4)).await.unwrap();
                assert_eq!(h.maven.client_metadata(r, "org/example/lib").await.unwrap(), Some(newer));
                assert!(h.maven.client_metadata(r, "org/example").await.unwrap().is_none());
                assert_eq!(h.maven.counter(r, GA).await.unwrap().value, 2);
            }
        }
    };
}

#[allow(unused_imports)]
pub(crate) use maven_contract;
