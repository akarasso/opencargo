use super::*;
use crate::proxy::engine::fixture::Fx;

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
async fn record_is_noop_without_rules() {
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
    let config = crate::config::Config::default();
    let state = crate::server::build_state(&crate::config::Config {
        server: crate::config::ServerConfig {
            storage_path: fx.storage.resolve("").unwrap().display().to_string(),
            ..Default::default()
        },
        database: crate::config::DatabaseConfig {
            url: format!(
                "sqlite:{}?mode=rwc",
                fx.storage.resolve("s.db").unwrap().display()
            ),
        },
        ..config
    })
    .await
    .unwrap();
    let cx = Cx {
        state: &state,
        auth: None,
        url: crate::registry::resolve::UrlRepo("requested"),
    };
    let ran = std::cell::Cell::new(false);
    record(
        &cx,
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
    assert_eq!(state.policy.dropped(), 0);
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
