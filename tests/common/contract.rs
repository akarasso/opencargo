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
                    handles.repos.delete_empty("npm-hosted").await.unwrap_err(),
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

                handles.repos.delete_empty("npm-hosted").await.unwrap();
                assert!(handles.repos.by_name("npm-hosted").await.unwrap().is_none());
                assert!(matches!(
                    handles.repos.delete_empty("npm-hosted").await.unwrap_err(),
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

                store.delete_version(landed.version.id).await.unwrap();

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
                    store.delete_version(landed.version.id).await.unwrap_err(),
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
    /// The repository an image's rows hang off; `OciStore` keys on it and
    /// carries no release of its own.
    pub repository: i64,
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
    pub repository: i64,
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
            repository: ports.repository,
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
            use ::opencargo::ports::oci::{NewManifest, Orphaned};
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

            fn manifest<'a>(
                repository: i64,
                digest: &'a str,
                blobs: &'a [String],
                tag: Option<&'a str>,
            ) -> NewManifest<'a> {
                NewManifest {
                    repository,
                    name: "app",
                    digest,
                    content_type: "application/vnd.oci.image.manifest.v1+json",
                    size: 2,
                    blobs,
                    tag,
                }
            }

            /// The orphan set is the point of the method: a layer another
            /// manifest still lists is not in it, and a layer nothing lists
            /// any more is — reported, never deleted from storage, because
            /// the store may not touch a file.
            #[tokio::test]
            async fn a_manifest_delete_reports_only_the_layers_it_orphaned() {
                let handles = $open().await;
                let repo = handles.repository;
                let (shared, only) = ("sha256:cc".to_string(), "sha256:bb".to_string());
                handles
                    .oci
                    .put_manifest(manifest(repo, "sha256:aa", &[only.clone(), shared.clone()], Some("v1")))
                    .await
                    .unwrap();
                handles
                    .oci
                    .put_manifest(manifest(repo, "sha256:dd", std::slice::from_ref(&shared), Some("v2")))
                    .await
                    .unwrap();

                let orphaned = handles
                    .oci
                    .delete_manifest(repo, "app", "sha256:aa")
                    .await
                    .unwrap()
                    .expect("the manifest was there");

                assert_eq!(orphaned, Orphaned { blob_digests: vec![only] });
                assert!(handles.oci.manifest(repo, "app", "sha256:aa").await.unwrap().is_none());
                assert!(handles.oci.digest_for_ref(repo, "app", "v1").await.unwrap().is_none());
                assert_eq!(handles.oci.blob_references(repo, &shared).await.unwrap(), 1);
                assert_eq!(handles.oci.tags(repo, "app").await.unwrap(), vec!["v2".to_string()]);
            }

            /// Nothing to delete is `None`, not an empty orphan set: the
            /// layer above turns one into a 404 and the other into a 202.
            #[tokio::test]
            async fn deleting_an_unknown_manifest_reports_nothing() {
                let handles = $open().await;
                assert!(handles
                    .oci
                    .delete_manifest(handles.repository, "app", "sha256:aa")
                    .await
                    .unwrap()
                    .is_none());
            }

            /// A re-push replaces the link rows wholesale, so a layer the new
            /// manifest no longer lists stops being referenced — otherwise a
            /// dropped layer would be undeletable for ever.
            #[tokio::test]
            async fn a_re_push_replaces_the_layers_and_moves_the_tag() {
                let handles = $open().await;
                let repo = handles.repository;
                let (old, new) = ("sha256:bb".to_string(), "sha256:cc".to_string());
                handles
                    .oci
                    .put_manifest(manifest(repo, "sha256:aa", std::slice::from_ref(&old), Some("v1")))
                    .await
                    .unwrap();
                handles
                    .oci
                    .put_manifest(manifest(repo, "sha256:aa", std::slice::from_ref(&new), Some("v1")))
                    .await
                    .unwrap();

                assert_eq!(handles.oci.blob_references(repo, &old).await.unwrap(), 0);
                assert_eq!(handles.oci.blob_references(repo, &new).await.unwrap(), 1);
                assert_eq!(handles.oci.tags(repo, "app").await.unwrap(), vec!["v1".to_string()]);
            }

            /// The ledger is what tells a chunk which repository it belongs
            /// to, and a completed upload closes it in the same breath as it
            /// records the blob.
            #[tokio::test]
            async fn an_upload_is_owned_until_it_completes() {
                let handles = $open().await;
                let repo = handles.repository;
                handles.oci.start_upload("u1", repo, "app").await.unwrap();
                assert_eq!(handles.oci.upload_owner("u1").await.unwrap(), Some(repo));

                handles
                    .oci
                    .complete_upload(
                        "u1",
                        ::opencargo::ports::oci::NewBlob {
                            repository: repo,
                            digest: "sha256:bb",
                            size: 5,
                            content_type: "application/octet-stream",
                        },
                    )
                    .await
                    .unwrap();

                assert!(handles.oci.upload_owner("u1").await.unwrap().is_none());
                assert_eq!(handles.oci.blob(repo, "sha256:bb").await.unwrap().unwrap().size, 5);
                assert!(handles.oci.delete_blob(repo, "sha256:bb").await.unwrap());
                assert!(!handles.oci.delete_blob(repo, "sha256:bb").await.unwrap());
            }
        }
    };
}

#[allow(unused_imports)]
pub(crate) use cascade_contract;
