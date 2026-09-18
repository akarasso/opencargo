use axum::response::IntoResponse;
use chrono::DateTime;

use super::*;
use crate::domain::{Format, RepoSpec, Visibility};
use crate::testing::fakes::{FakeDb, PortId};
use crate::testing::resolver::{user, Resolver};

struct Script;

/// The member's name is the script: the walk's order, dedup and verdicts are
/// what is under test, never what a format does with a hit.
fn scripted(repo: &Repository) -> Result<Outcome<String>, ResolveError> {
    match repo.name.as_str() {
        n if n.ends_with("-miss") => Ok(Outcome::NotFound),
        n if n.ends_with("-err404") => Err(ResolveError::NotFound(n.to_string())),
        n if n.ends_with("-fail") => Err(ResolveError::Upstream(format!("{n} is down"))),
        n => Ok(Outcome::Found(n.to_string())),
    }
}

#[async_trait::async_trait]
impl Leaf for Script {
    type Out = String;

    async fn hosted(
        &self,
        _cx: &Cx<'_>,
        m: CacheRepo<'_>,
    ) -> Result<Outcome<String>, ResolveError> {
        scripted(m.0)
    }

    async fn proxy(
        &self,
        _cx: &Cx<'_>,
        m: CacheRepo<'_>,
        _up: &Upstream,
    ) -> Result<Outcome<String>, ResolveError> {
        scripted(m.0)
    }
}

fn spec<'a>(name: &'a str, kind: RepoKind, members: &'a [String]) -> RepoSpec<'a> {
    RepoSpec {
        name,
        kind,
        format: Format::Npm,
        visibility: Visibility::Public,
        upstream: (kind == RepoKind::Proxy).then_some("http://127.0.0.1:1"),
        members,
    }
}

async fn add(db: &FakeDb, name: &str, kind: RepoKind, members: &[&str]) {
    let members: Vec<String> = members.iter().map(|m| m.to_string()).collect();
    db.repositories()
        .create(&spec(name, kind, &members), DateTime::UNIX_EPOCH)
        .await
        .unwrap();
}

/// The same repositories the temp-database fixture used to insert, including
/// the six rows write-time validation now refuses and the resolver must still
/// tolerate: a missing member, a member of another format, two cycles and a
/// stack one level past the cap.
async fn fixture() -> Resolver {
    let db = FakeDb::new();
    for hosted in ["h-found", "h-miss", "h-err404", "h-fail"] {
        add(&db, hosted, RepoKind::Hosted, &[]).await;
    }
    add(&db, "p-found", RepoKind::Proxy, &[]).await;
    db.repositories()
        .create(
            &RepoSpec {
                visibility: Visibility::Private,
                ..spec("h-private", RepoKind::Hosted, &[])
            },
            DateTime::UNIX_EPOCH,
        )
        .await
        .unwrap();
    db.repositories()
        .create(
            &RepoSpec {
                format: Format::Cargo,
                ..spec("cargo-found", RepoKind::Hosted, &[])
            },
            DateTime::UNIX_EPOCH,
        )
        .await
        .unwrap();

    let groups: [(&str, &[&str]); 8] = [
        ("g-order", &["h-miss", "h-err404", "h-found", "p-found"]),
        ("g-fail-then-found", &["h-fail", "h-found"]),
        ("g-fail-only", &["h-fail", "h-miss"]),
        ("g-nested", &["g-order"]),
        ("g-skips", &["nope", "h-private", "cargo-found", "h-miss"]),
        ("g-empty", &[]),
        ("g-cycle-a", &["g-cycle-b"]),
        ("g-cycle-b", &["g-cycle-a", "p-found"]),
    ];
    for (name, members) in groups {
        add(&db, name, RepoKind::Group, members).await;
    }
    // d0 -> d1 -> .. -> d6 -> h-found: d2 is the deepest a write allows.
    for i in 0..7 {
        let next = if i == 6 {
            "h-found".to_string()
        } else {
            format!("d{}", i + 1)
        };
        add(&db, &format!("d{i}"), RepoKind::Group, &[&next]).await;
    }
    Resolver::new(db, Default::default())
}

async fn repo(fx: &Resolver, name: &str) -> Repository {
    fx.fakes
        .repositories()
        .by_name(name)
        .await
        .unwrap()
        .expect("the fixture declares it")
}

async fn first(fx: &Resolver, name: &str) -> Result<String, ResolveError> {
    first_hit(&fx.cx(None, "requested"), &repo(fx, name).await, &Script).await
}

async fn all(fx: &Resolver, name: &str) -> Result<Collected<String>, ResolveError> {
    collect(&fx.cx(None, "requested"), &repo(fx, name).await, &Script).await
}

fn status(err: ResolveError) -> axum::http::StatusCode {
    AppError::from(err).into_response().status()
}

#[tokio::test]
async fn error_policy_table() {
    let fx = fixture().await;

    assert_eq!(first(&fx, "h-found").await.unwrap(), "h-found");
    assert_eq!(first(&fx, "p-found").await.unwrap(), "p-found");
    assert!(matches!(
        first(&fx, "h-miss").await,
        Err(ResolveError::NotFound(_))
    ));
    assert!(matches!(
        first(&fx, "h-fail").await,
        Err(ResolveError::Upstream(_))
    ));
    assert!(matches!(
        all(&fx, "h-fail").await,
        Err(ResolveError::Upstream(_))
    ));

    assert_eq!(first(&fx, "g-order").await.unwrap(), "h-found");
    let ordered = all(&fx, "g-order").await.unwrap();
    assert_eq!(ordered.hits, vec!["h-found", "p-found"]);
    assert_eq!(ordered.degraded, None);

    assert_eq!(first(&fx, "g-fail-then-found").await.unwrap(), "h-found");
    let degraded = all(&fx, "g-fail-then-found").await.unwrap();
    assert_eq!(degraded.hits, vec!["h-found"]);
    assert!(degraded.degraded.unwrap().contains("h-fail is down"));

    let Err(ResolveError::Upstream(msg)) = first(&fx, "g-fail-only").await else {
        panic!("no hit after a failure is 502");
    };
    assert!(msg.contains("group requested") && msg.contains("h-fail"), "{msg}");
    assert!(matches!(
        all(&fx, "g-fail-only").await,
        Err(ResolveError::Upstream(_))
    ));

    assert!(matches!(
        first(&fx, "g-skips").await,
        Err(ResolveError::NotFound(_))
    ));
    let skipped = all(&fx, "g-skips").await.unwrap();
    assert!(skipped.hits.is_empty() && skipped.degraded.is_none());
    assert!(matches!(
        first(&fx, "g-empty").await,
        Err(ResolveError::NotFound(_))
    ));
    assert!(all(&fx, "g-empty").await.unwrap().hits.is_empty());

    assert_eq!(first(&fx, "g-cycle-a").await.unwrap(), "p-found");
    assert_eq!(all(&fx, "g-cycle-b").await.unwrap().hits, vec!["p-found"]);
    assert_eq!(first(&fx, "g-nested").await.unwrap(), "h-found");

    assert_eq!(first(&fx, "d2").await.unwrap(), "h-found");
    for too_deep in ["d1", "d0"] {
        let Err(ResolveError::Internal(msg)) = first(&fx, too_deep).await else {
            panic!("{too_deep}: a sixth nested group exceeds the depth cap");
        };
        assert!(msg.contains("depth"), "{msg}");
    }
}

/// The depth cap is a row write-time validation should have refused, not
/// something the client can act on.
#[tokio::test]
async fn the_depth_cap_is_a_five_hundred() {
    let fx = fixture().await;
    let err = first(&fx, "d0").await.unwrap_err();
    assert_eq!(status(err), axum::http::StatusCode::INTERNAL_SERVER_ERROR);
}

/// The grant lookup failing is never a verdict. `Other` is the injection that
/// proves it: `Unavailable` would pass whether or not the collapse exists.
#[tokio::test]
async fn a_failing_grant_lookup_is_a_retryable_five_oh_three() {
    let fx = fixture().await;
    let alice = user(1, "reader");
    fx.fakes.fail_next(
        PortId::Permissions,
        StoreError::Other(Box::new(std::io::Error::other("disk gone"))),
    );
    let group = repo(&fx, "g-skips").await;
    let err = first_hit(&fx.cx(Some(&alice), "requested"), &group, &Script)
        .await
        .unwrap_err();
    assert!(
        matches!(err, ResolveError::Store(StoreError::Unavailable)),
        "{err}"
    );
    assert_eq!(status(err), axum::http::StatusCode::SERVICE_UNAVAILABLE);
}

#[test]
fn every_status_the_resolver_serves() {
    let cases = [
        (ResolveError::NotFound("x".into()), 404),
        (ResolveError::Upstream("x".into()), 502),
        (
            ResolveError::Domain(DomainError::InvalidName("x".into())),
            400,
        ),
        (ResolveError::Store(StoreError::NotFound), 404),
        (ResolveError::Store(StoreError::Conflict), 409),
        (ResolveError::Store(StoreError::Unavailable), 503),
        (
            ResolveError::Store(StoreError::Other(Box::new(std::io::Error::other("x")))),
            500,
        ),
        (ResolveError::Internal("x".into()), 500),
    ];
    for (err, expected) in cases {
        assert_eq!(status(err).as_u16(), expected);
    }
}

/// The engine still answers in `AppError`; nothing it can return may change
/// the status the client already gets today.
#[test]
fn the_engines_vocabulary_crosses_without_changing_a_status() {
    let cases = [
        (AppError::NotFound("x".into()), 404),
        (AppError::BadGateway("x".into()), 502),
        (AppError::Internal("x".into()), 500),
        (AppError::ServiceUnavailable("x".into()), 503),
        (AppError::Io(std::io::Error::other("x")), 500),
    ];
    for (err, expected) in cases {
        assert_eq!(status(ResolveError::from(err)).as_u16(), expected);
    }
}
