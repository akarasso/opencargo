use super::*;
use crate::proxy::engine::fixture::{timeouts, Fx};

pub fn repo(id: i64, name: &str, format: Format) -> Repository {
    Repository {
        id,
        name: name.into(),
        repo_type: "proxy".into(),
        format: format.as_str().into(),
        visibility: "public".into(),
        upstream_url: Some("http://127.0.0.1:1/".into()),
        config_json: None,
        created_at: String::new(),
        updated_at: String::new(),
    }
}

pub fn pending(
    member: &Repository,
    up: &Upstream,
    format: Format,
    name: &str,
    source: Source,
) -> Pending {
    Pending {
        requested_repo: "requested".into(),
        member: member.clone(),
        upstream: up.clone(),
        format,
        name: name.into(),
        version: Some("1.0.0".into()),
        actor: Actor::of(None),
        source,
    }
}

/// An engine over the proxy fixture: its member records when `cfg` is on.
pub fn engine_over(
    fx: &Fx,
    cfg: PolicyConfig,
    tuning: Tuning,
) -> (PolicyEngine, impl Future<Output = ()>) {
    let config = HashMap::from([(fx.repo.name.clone(), cfg)]);
    PolicyEngine::unspawned(
        fx.pool.clone(),
        &config,
        Arc::new(EventBus::new()),
        fx.engine(timeouts()),
        tuning,
    )
}

pub fn fast() -> Tuning {
    Tuning {
        child_ttl: Duration::from_millis(200),
        pacer_period: Duration::from_millis(20),
        pacer_cooldown: Duration::from_millis(200),
        gather_timeout: Duration::from_secs(2),
        notify_period: Duration::from_millis(50),
        refresh_floor: Duration::from_secs(60),
    }
}
