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
    pub deps: Arc<dyn DependencyStore>,
    _keep: Box<dyn Any + Send>,
}

/// The handle set an adapter exposes, which is the same set on both sides of
/// the boundary — that sameness is what lets one suite run against each.
pub struct Ports {
    pub webhooks: Arc<dyn WebhookStore>,
    pub repos: Arc<dyn RepositoryStore>,
    pub packages: Arc<dyn PackageStore>,
    pub search: Arc<dyn SearchIndex>,
    pub deps: Arc<dyn DependencyStore>,
}

impl Handles {
    pub fn new(ports: Ports, keep: Box<dyn Any + Send>) -> Self {
        Self {
            webhooks: ports.webhooks,
            repos: ports.repos,
            packages: ports.packages,
            search: ports.search,
            deps: ports.deps,
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
                    dependencies: &[],
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

            /// The edges of a release are written with its version row, so
            /// a visible version never lacks them; a refused one writes none.
            #[tokio::test]
            async fn a_release_lands_with_its_dependencies_or_not_at_all() {
                use ::opencargo::ports::packages::ReleaseDependency;
                let handles = $open().await;
                let repo = hosted(&handles, "npm-hosted", Visibility::Public).await;
                let edges = [
                    ReleaseDependency { name: "a", requirement: "^1", kind: "net8.0" },
                    ReleaseDependency { name: "b", requirement: "", kind: "any" },
                ];
                let mut with = release(repo.id, "left-pad", "1.0.0", &[]);
                with.dependencies = &edges;
                let landed = handles.packages.publish_version(&with).await.unwrap();
                let recorded = handles.deps.of_version(landed.version.id).await.unwrap();
                assert_eq!(
                    recorded
                        .iter()
                        .map(|d| (d.name.as_str(), d.requirement.as_str(), d.kind.as_str()))
                        .collect::<Vec<_>>(),
                    [("a", "^1", "net8.0"), ("b", "", "any")]
                );

                let refused = handles.packages.publish_version(&with).await.unwrap_err();
                assert!(matches!(refused, StoreError::Conflict), "{refused:?}");
                assert_eq!(handles.deps.of_version(landed.version.id).await.unwrap().len(), 2);
                assert!(
                    handles.deps.dependents("a", false).await.unwrap().len() == 1,
                    "the refused publish recorded no second edge"
                );
            }

            /// A1 C5: each publish, yank, unyank and delete moves the stamp,
            /// nothing else does, and a stamp is a function of the versions'
            /// state, so a validated reader never sees a stale state as current.
            #[tokio::test]
            async fn the_version_stamp_moves_with_the_four_operations_only() {
                let handles = $open().await;
                let repo = hosted(&handles, "npm-hosted", Visibility::Public).await;
                let first = handles
                    .packages
                    .publish_version(&release(repo.id, "left-pad", "1.0.0", &[]))
                    .await
                    .unwrap();
                let package = first.package.id;
                let stamp = || handles.packages.stamp(package);
                let one = stamp().await.unwrap();
                let second = handles
                    .packages
                    .publish_version(&release(repo.id, "left-pad", "2.0.0", &[]))
                    .await
                    .unwrap();
                let two = stamp().await.unwrap();
                assert_ne!(two, one, "publish");
                handles.packages.set_yanked(first.version.id, true).await.unwrap();
                let yanked = stamp().await.unwrap();
                assert_ne!(yanked, two, "yank");
                handles.packages.set_yanked(first.version.id, false).await.unwrap();
                let unyanked = stamp().await.unwrap();
                assert_ne!(unyanked, yanked, "unyank");
                assert_eq!(unyanked, two, "the same state, the same stamp");
                handles.packages.set_metadata(second.version.id, "{\"x\":1}").await.unwrap();
                handles.packages.record_download(second.version.id).await.unwrap();
                assert_eq!(stamp().await.unwrap(), two, "nothing else moves it");
                handles.packages.delete_version(second.version.id, at(10)).await.unwrap();
                let deleted = stamp().await.unwrap();
                assert_ne!(deleted, two, "delete");
                handles
                    .packages
                    .publish_version(&release(repo.id, "left-pad", "2.0.0", &[]))
                    .await
                    .unwrap();
                let republished = stamp().await.unwrap();
                assert_ne!(republished, deleted, "republish");
                assert_ne!(republished, two, "a republished version is a new row");
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

/// What `reclaim_contract!` needs of an adapter: port 22, port 23, and the
/// ports whose rows reference keys or whose methods enqueue.
pub struct ReclaimHandles {
    pub repos: Arc<dyn RepositoryStore>,
    pub packages: Arc<dyn PackageStore>,
    pub oci: Arc<dyn OciStore>,
    pub reclaim: Arc<dyn opencargo::ports::reclaim::ReclaimStore>,
    pub referenced: Arc<dyn opencargo::ports::referenced::ReferencedKeys>,
    _keep: Box<dyn Any + Send>,
}

impl ReclaimHandles {
    pub fn new(
        repos: Arc<dyn RepositoryStore>,
        packages: Arc<dyn PackageStore>,
        oci: Arc<dyn OciStore>,
        reclaim: Arc<dyn opencargo::ports::reclaim::ReclaimStore>,
        referenced: Arc<dyn opencargo::ports::referenced::ReferencedKeys>,
        keep: Box<dyn Any + Send>,
    ) -> Self {
        Self {
            repos,
            packages,
            oci,
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
                        dependencies: &[],
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

            /// NuGet 2.5: the `.nupkg` of an unlisted version stays referenced,
            /// because an unlist keeps it restorable by exact version.
            #[tokio::test]
            async fn an_unlisted_nupkg_is_referenced_and_listed() {
                let h = $open().await;
                let (r, prefix) = repo(&h, "nuget").await;
                let key = format!("{prefix}/my.lib/ab/my.lib.1.0.0.nupkg~g1");
                let id = publish(&h, r.id, "1.0.0", &key).await;
                h.packages.set_yanked(id, true).await.unwrap();
                h.reclaim.enqueue(std::slice::from_ref(&key), at(1)).await.unwrap();
                assert!(listed(&h, 5).await.contains(&key));
                assert_eq!(claim(&h, &key, 5).await, Claim::Referenced);
            }

            #[tokio::test]
            async fn segments_of_a_slow_session_are_never_claimable() {
                let h = $open().await;
                let (r, _) = repo(&h, "r").await;
                h.oci.start_upload("u1", r.id, "app").await.unwrap();
                let segment = "oci/_uploads/u1/00000000000000000000".to_string();
                h.reclaim.enqueue(std::slice::from_ref(&segment), at(1)).await.unwrap();
                assert_eq!(claim(&h, &segment, 20).await, Claim::Referenced);
                let refs: Vec<_> = h.referenced.referenced(GRACE, at(20)).try_collect().await.unwrap();
                assert!(refs.iter().any(|r| r.key == "oci/_uploads/u1" && r.prefix));
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
                    dependencies: &[],
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
            async fn forgetting_a_retired_prefix_keeps_the_rest_retired() {
                let h = $open().await;
                let (_, prefix) = repo(&h, "r").await;
                h.repos.retire("r", at(2)).await.unwrap();
                h.reclaim.forget_retired("npm/r").await.unwrap();
                let keys = vec!["k".to_string()];
                assert_eq!(h.reclaim.pin(&prefix, &keys, at(3)).await.unwrap(), Pinned::Retired);
            }
        }
    };
}

#[allow(unused_imports)]
pub(crate) use reclaim_contract;

/// What `NugetFeedRead` (port 17) owes, beside the `PackageStore` it reads.
pub struct FeedHandles {
    pub repos: Arc<dyn RepositoryStore>,
    pub packages: Arc<dyn PackageStore>,
    pub feed: Arc<dyn opencargo::ports::nuget::NugetFeedRead>,
    _keep: Box<dyn Any + Send>,
}

impl FeedHandles {
    pub fn new(
        repos: Arc<dyn RepositoryStore>,
        packages: Arc<dyn PackageStore>,
        feed: Arc<dyn opencargo::ports::nuget::NugetFeedRead>,
        keep: Box<dyn Any + Send>,
    ) -> Self {
        Self {
            repos,
            packages,
            feed,
            _keep: keep,
        }
    }
}

/// `nuget_feed_contract!(name, opener)`: port 17 filters before it cuts the
/// window, counts exactly, never returns an unlisted version, and holds a
/// consistent window while publishes land; `PackageStore` gives two
/// concurrent publishes of one normalized version one row and one
/// `Conflict`.
#[allow(unused_macros)]
macro_rules! nuget_feed_contract {
    ($suite:ident, $open:path) => {
        mod $suite {
            use super::*;
            use ::chrono::Utc;
            use ::opencargo::domain::{Format, RepoKind, RepoSpec, Visibility};
            use ::opencargo::error::StoreError;
            use ::opencargo::ports::nuget::FeedQuery;
            use ::opencargo::ports::packages::{NameMatch, NewRelease};

            async fn repo(h: &FeedHandles) -> i64 {
                h.repos
                    .create(
                        &RepoSpec {
                            name: "nuget-hosted",
                            kind: RepoKind::Hosted,
                            format: Format::Nuget,
                            visibility: Visibility::Public,
                            upstream: None,
                            members: &[],
                        },
                        Utc::now(),
                    )
                    .await
                    .unwrap()
                    .id
            }

            async fn publish(
                h: &FeedHandles,
                repository: i64,
                package: &str,
                version: &str,
                facts: &str,
            ) -> Result<i64, StoreError> {
                h.packages
                    .publish_version(&NewRelease {
                        repository,
                        package,
                        match_name: NameMatch::Exact,
                        description: Some("a nuget package"),
                        readme: None,
                        version,
                        metadata_json: facts,
                        checksum_sha1: None,
                        checksum_sha256: None,
                        integrity: None,
                        size: 1,
                        tarball_path: "k",
                        dist_tags: &[],
                        dependencies: &[],
                        pins: &[],
                        now: Utc::now(),
                    })
                    .await
                    .map(|r| r.version.id)
            }

            fn query(repository: i64) -> FeedQuery<'static> {
                FeedQuery {
                    repository,
                    take: 20,
                    ..FeedQuery::default()
                }
            }

            #[tokio::test]
            async fn filters_apply_before_the_window_and_the_total_is_exact() {
                let h = $open().await;
                let r = repo(&h).await;
                publish(&h, r, "a.pre", "1.0.0-beta", r#"{"prerelease":true}"#).await.unwrap();
                publish(&h, r, "b.stable", "1.0.0", "{}").await.unwrap();
                publish(&h, r, "c.pre", "2.0.0-rc", r#"{"prerelease":true}"#).await.unwrap();
                publish(&h, r, "d.stable", "1.0.0", r#"{"packageTypes":["dotnettool"]}"#)
                    .await
                    .unwrap();
                publish(&h, r, "e.semver2", "1.0.0-rc.1", r#"{"prerelease":true,"semver2":true}"#)
                    .await
                    .unwrap();

                let stable = h.feed.search(&FeedQuery { take: 1, ..query(r) }).await.unwrap();
                assert_eq!(stable.total, 2);
                assert_eq!(stable.hits.len(), 1);
                assert_eq!(stable.hits[0].package.name, "b.stable");
                let second = h.feed.search(&FeedQuery { skip: 1, take: 1, ..query(r) }).await.unwrap();
                assert_eq!(second.hits[0].package.name, "d.stable");

                let pre = h.feed.search(&FeedQuery { prerelease: true, ..query(r) }).await.unwrap();
                assert_eq!(pre.total, 4, "semver2 still filtered");
                let all = h
                    .feed
                    .search(&FeedQuery { prerelease: true, semver2: true, ..query(r) })
                    .await
                    .unwrap();
                assert_eq!(all.total, 5);
                let tools = h
                    .feed
                    .search(&FeedQuery { package_type: Some("DotnetTool"), ..query(r) })
                    .await
                    .unwrap();
                assert_eq!(tools.total, 1);
                let text = h
                    .feed
                    .search(&FeedQuery { text: Some("STABLE"), ..query(r) })
                    .await
                    .unwrap();
                assert_eq!(text.total, 2);
            }

            #[tokio::test]
            async fn an_unlisted_version_is_never_returned() {
                let h = $open().await;
                let r = repo(&h).await;
                let old = publish(&h, r, "lib", "1.0.0", "{}").await.unwrap();
                publish(&h, r, "lib", "2.0.0", "{}").await.unwrap();
                let only = publish(&h, r, "solo", "1.0.0", "{}").await.unwrap();
                h.packages.set_yanked(old, true).await.unwrap();
                h.packages.set_yanked(only, true).await.unwrap();

                let page = h.feed.search(&query(r)).await.unwrap();
                assert_eq!(page.total, 1, "a package with nothing listed is not a hit");
                let versions: Vec<&str> =
                    page.hits[0].versions.iter().map(|v| v.version.as_str()).collect();
                assert_eq!(versions, ["2.0.0"]);
            }

            #[tokio::test]
            async fn two_spellings_of_one_version_race_to_one_row() {
                let h = $open().await;
                let r = repo(&h).await;
                let (a, b) = tokio::join!(
                    publish(&h, r, "race", "1.0.0", "{}"),
                    publish(&h, r, "race", "1.0.0", "{}")
                );
                let conflicts = [&a, &b]
                    .iter()
                    .filter(|x| matches!(x, Err(StoreError::Conflict)))
                    .count();
                assert_eq!(conflicts, 1, "{a:?} {b:?}");
                assert!([&a, &b].iter().any(|x| x.is_ok()));
                let page = h.feed.search(&query(r)).await.unwrap();
                assert_eq!(page.hits[0].versions.len(), 1);
            }

            #[tokio::test]
            async fn the_window_holds_while_publishes_land() {
                let h = $open().await;
                let r = repo(&h).await;
                let writer = async {
                    for i in 0..20 {
                        publish(&h, r, &format!("p{i:02}"), "1.0.0", "{}").await.unwrap();
                    }
                };
                let reader = async {
                    let mut last = 0;
                    for _ in 0..20 {
                        let page = h.feed.search(&FeedQuery { take: 5, ..query(r) }).await.unwrap();
                        assert!(page.total >= last, "the total never goes back");
                        assert_eq!(page.hits.len() as u64, page.total.min(5));
                        let names: Vec<&str> =
                            page.hits.iter().map(|h| h.package.name.as_str()).collect();
                        let mut sorted = names.clone();
                        sorted.sort();
                        assert_eq!(names, sorted);
                        last = page.total;
                        tokio::task::yield_now().await;
                    }
                };
                tokio::join!(writer, reader);
                assert_eq!(h.feed.search(&query(r)).await.unwrap().total, 20);
            }
        }
    };
}

#[allow(unused_imports)]
pub(crate) use nuget_feed_contract;
