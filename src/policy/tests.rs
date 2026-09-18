use super::*;
use crate::testing::fakes::FakeDb;
use crate::testing::fixture::Fx;
use crate::testing::resolver::{Recorder, Resolver};

fn user(user_id: Option<i64>, username: &str, token_name: Option<&str>) -> AuthUser {
    AuthUser {
        token: String::new(),
        user_id,
        username: username.into(),
        role: "reader".into(),
        must_change_password: false,
        token_name: token_name.map(String::from),
    }
}

#[test]
fn actor_of_token_user_static_anonymous() {
    let token = Actor::of(Some(&user(Some(3), "ci", Some("ci-runner"))));
    assert_eq!(
        (token.name.as_str(), token.kind, token.user_id),
        ("ci-runner", ActorKind::Token, Some(3))
    );
    let basic = Actor::of(Some(&user(Some(4), "alice", None)));
    assert_eq!(
        (basic.name.as_str(), basic.kind, basic.user_id),
        ("alice", ActorKind::User, Some(4))
    );
    let fixed = Actor::of(Some(&user(None, "static-token", None)));
    assert_eq!(
        (fixed.name.as_str(), fixed.kind, fixed.user_id),
        ("static-token", ActorKind::Static, None)
    );
    let nobody = Actor::of(None);
    assert_eq!(
        (nobody.name.as_str(), nobody.kind, nobody.user_id),
        ("anonymous", ActorKind::Anonymous, None)
    );
}

#[tokio::test]
async fn record_runs_only_for_a_watched_member() {
    let fx = Fx::new().await;
    let (engine, _writer) = testing::engine_over(&fx, PolicyConfig::default(), Tuning::default());
    assert!(!engine.records("p"));
    assert!(!engine.records("other"));
    let (on, _writer) = testing::engine_over(
        &fx,
        PolicyConfig {
            typosquat: true,
            ..Default::default()
        },
        Tuning::default(),
    );
    assert!(on.records("p"));
    assert!(!on.records("other"));
    // The free function's own short-circuit, against a recorder that watches
    // nobody: no engine, no queue and no database behind it.
    let resolver = Resolver::default();
    let ran = std::cell::Cell::new(false);
    record(
        &resolver.cx(None, "requested"),
        CacheRepo(&fx.repo),
        &fx.up,
        Format::Npm,
        "lodash",
        None,
        || {
            ran.set(true);
            Source::Cargo {
                cksum: String::new(),
            }
        },
    );
    assert!(
        !ran.get(),
        "the source closure never runs for an unconfigured member"
    );
    assert!(resolver.policy.recorded().is_empty());

    let watching = Resolver::new(
        FakeDb::new(),
        Recorder::watching(&[fx.repo.name.as_str()]),
    );
    let ran = std::cell::Cell::new(false);
    record(
        &watching.cx(None, "requested"),
        CacheRepo(&fx.repo),
        &fx.up,
        Format::Npm,
        "lodash",
        None,
        || {
            ran.set(true);
            Source::Cargo {
                cksum: String::new(),
            }
        },
    );
    assert!(ran.get(), "a watched member runs it");
    assert_eq!(watching.policy.recorded(), vec![fx.repo.name.clone()]);
}

#[tokio::test]
async fn full_queue_drops_and_counts() {
    let fx = Fx::new().await;
    let (engine, writer) = testing::engine_over(
        &fx,
        PolicyConfig {
            typosquat: true,
            ..Default::default()
        },
        Tuning::default(),
    );
    for _ in 0..=QUEUE {
        engine.record(testing::pending(
            &fx.repo,
            &fx.up,
            Format::Cargo,
            "serde",
            Source::Cargo { cksum: "ab".into() },
        ));
    }
    assert_eq!(engine.dropped(), 1);
    assert_eq!(engine.queue_capacity(), 0);
    drop(writer);
}
