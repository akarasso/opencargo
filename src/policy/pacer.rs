use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::time::{interval, timeout, Interval, MissedTickBehavior};
use tracing::debug;

use crate::error::AppError;
use crate::proxy::engine::Cached;
use crate::proxy::UpstreamStrategy;
use crate::registry::cargo::upstream::{CargoArtifact, CargoUpstream};
use crate::registry::resolve::{CacheRepo, Outcome, Upstream};

use super::rules::PolicyConfig;
use super::Shared;

/// One request at a time per API host, a period apart, with a cooldown on
/// 429 and a waiting line that never outlives `gather_timeout`.
#[derive(Default)]
pub struct Pacer {
    lanes: Mutex<HashMap<String, Arc<Lane>>>,
}

pub(crate) struct Lane {
    pub gate: tokio::sync::Mutex<Interval>,
    cooldown_until: Mutex<Option<Instant>>,
    waiting: AtomicUsize,
}

impl Lane {
    fn cooling(&self) -> bool {
        self.cooldown_until
            .lock()
            .unwrap()
            .is_some_and(|until| until > Instant::now())
    }

    fn cool(&self, for_: Duration) {
        *self.cooldown_until.lock().unwrap() = Some(Instant::now() + for_);
    }
}

/// `transfer.rs` formats a refused status as `upstream answered 429 ...`.
pub fn is_throttled(e: &AppError) -> bool {
    matches!(e, AppError::BadGateway(why) if why.starts_with("upstream answered 429"))
}

impl Pacer {
    pub(crate) fn lane(&self, host: &str, period: Duration) -> Arc<Lane> {
        self.lanes
            .lock()
            .unwrap()
            .entry(host.to_string())
            .or_insert_with(|| {
                let mut gate = interval(period);
                gate.set_missed_tick_behavior(MissedTickBehavior::Delay);
                Arc::new(Lane {
                    gate: tokio::sync::Mutex::new(gate),
                    cooldown_until: Mutex::new(None),
                    waiting: AtomicUsize::new(0),
                })
            })
            .clone()
    }

    /// One paced `fetch`; `rate-limited` at once past the line cap or under
    /// a cooldown, and after `gather_timeout` in line.
    pub(crate) async fn fetch(
        &self,
        shared: &Shared,
        cfg: &PolicyConfig,
        member: CacheRepo<'_>,
        up: &Upstream,
        a: &CargoArtifact,
    ) -> (Option<Cached>, &'static str) {
        if !cfg.fetch_missing_facts {
            return (None, "not-fetched");
        }
        let tuning = shared.tuning;
        let host = CargoUpstream
            .upstream_url(up, a)
            .ok()
            .and_then(|url| url.host_str().map(String::from))
            .unwrap_or_default();
        let lane = self.lane(&host, tuning.pacer_period);
        if lane.cooling() || lane.waiting.load(Ordering::SeqCst) >= tuning.pacer_waiters() {
            return (None, "rate-limited");
        }
        lane.waiting.fetch_add(1, Ordering::SeqCst);
        let gate = timeout(tuning.gather_timeout, lane.gate.lock()).await;
        lane.waiting.fetch_sub(1, Ordering::SeqCst);
        let Ok(mut gate) = gate else {
            return (None, "rate-limited");
        };
        if lane.cooling() {
            return (None, "rate-limited");
        }
        gate.tick().await;
        let fetched = timeout(
            tuning.gather_timeout,
            shared.proxy.observe(&CargoUpstream, up, member, a),
        )
        .await;
        drop(gate);
        match fetched {
            Err(_) => (None, "timeout"),
            Ok(Ok(Outcome::Found(cached))) => (Some(cached), "fetch"),
            Ok(Ok(Outcome::NotFound)) => (None, "not-found"),
            Ok(Err(e)) if is_throttled(&e) => {
                lane.cool(tuning.pacer_cooldown);
                (None, "rate-limited")
            }
            Ok(Err(e)) => {
                debug!(host, error = %e, "crates.io version lookup failed");
                (None, "failed")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::testing::{engine_over, fast};
    use crate::policy::Tuning;
    use crate::proxy::engine::fixture::Fx;
    use axum::http::StatusCode;

    fn meta(fx: &Fx, version: &str) -> CargoArtifact {
        CargoArtifact::VersionMeta {
            api: fx.up.base.clone(),
            name: "serde".into(),
            version: version.into(),
        }
    }

    fn cfg() -> PolicyConfig {
        PolicyConfig {
            min_release_age: Some("1h".parse().unwrap()),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn one_in_flight_one_second_apart() {
        let fx = Fx::new().await;
        fx.set(|s| s.body = br#"{"version":{"created_at":"2026-01-01T00:00:00Z"}}"#.to_vec());
        let tuning = Tuning {
            pacer_period: Duration::from_millis(50),
            ..fast()
        };
        let (engine, _writer) = engine_over(&fx, cfg(), tuning);
        let shared = engine.shared();
        let mut up = fx.up.clone();
        up.dl_allow_private = true;
        let fetches = (0..10).map(|i| {
            let a = meta(&fx, &format!("1.0.{i}"));
            let (shared, up, repo) = (shared.clone(), up.clone(), fx.repo.clone());
            async move {
                let (cached, source) = shared
                    .cargo_pacer
                    .fetch(&shared, &cfg(), CacheRepo(&repo), &up, &a)
                    .await;
                assert_eq!(source, "fetch");
                cached.unwrap();
            }
        });
        futures_util::future::join_all(fetches).await;
        let starts: Vec<Instant> = fx.fake.lock().unwrap().starts.clone();
        assert_eq!(starts.len(), 10);
        for pair in starts.windows(2) {
            assert!(pair[1] >= pair[0] + Duration::from_millis(49), "{pair:?}");
        }
    }

    #[tokio::test]
    async fn throttled_sets_cooldown_and_rate_limited() {
        let fx = Fx::new().await;
        fx.set(|s| s.status = Some(StatusCode::TOO_MANY_REQUESTS));
        let (engine, _writer) = engine_over(&fx, cfg(), fast());
        let shared = engine.shared();
        let mut up = fx.up.clone();
        up.dl_allow_private = true;
        let member = CacheRepo(&fx.repo);
        let pacer = &shared.cargo_pacer;
        let (cached, source) = pacer
            .fetch(shared, &cfg(), member, &up, &meta(&fx, "1"))
            .await;
        assert!(cached.is_none());
        assert_eq!(source, "rate-limited");
        fx.set(|s| s.status = None);
        fx.set(|s| s.body = br#"{"version":{}}"#.to_vec());
        let (_, source) = pacer
            .fetch(shared, &cfg(), member, &up, &meta(&fx, "2"))
            .await;
        assert_eq!(source, "rate-limited", "under cooldown, no request");
        assert_eq!(fx.hits().len(), 1);
        tokio::time::sleep(fast().pacer_cooldown + Duration::from_millis(20)).await;
        let (cached, source) = pacer
            .fetch(shared, &cfg(), member, &up, &meta(&fx, "2"))
            .await;
        assert_eq!(source, "fetch");
        assert!(cached.is_some());
        assert_eq!(fx.hits().len(), 2);
        let off = PolicyConfig {
            fetch_missing_facts: false,
            ..cfg()
        };
        assert_eq!(
            pacer
                .fetch(shared, &off, member, &up, &meta(&fx, "3"))
                .await
                .1,
            "not-fetched"
        );
        assert_eq!(fx.hits().len(), 2);
    }

    #[tokio::test]
    async fn waiters_beyond_cap_are_rate_limited_at_once() {
        assert_eq!(Tuning::default().pacer_waiters(), 15);
        let tuning = Tuning {
            gather_timeout: Duration::from_secs(2),
            pacer_period: Duration::from_millis(500),
            ..fast()
        };
        assert_eq!(tuning.pacer_waiters(), 4);
        let fx = Fx::new().await;
        let (engine, _writer) = engine_over(&fx, cfg(), tuning);
        let shared = engine.shared().clone();
        let host = fx.up.base.host_str().unwrap().to_string();
        let lane = shared.cargo_pacer.lane(&host, tuning.pacer_period);
        let held = lane.gate.lock().await;
        let mut up = fx.up.clone();
        up.dl_allow_private = true;
        let mut waiters = Vec::new();
        for i in 0..4 {
            let (shared, up, repo) = (shared.clone(), up.clone(), fx.repo.clone());
            waiters.push(tokio::spawn(async move {
                let a = meta_for(&up, &format!("2.0.{i}"));
                shared
                    .cargo_pacer
                    .fetch(&shared, &cfg(), CacheRepo(&repo), &up, &a)
                    .await
                    .1
            }));
        }
        for _ in 0..20 {
            tokio::task::yield_now().await;
        }
        let started = Instant::now();
        let (_, source) = shared
            .cargo_pacer
            .fetch(&shared, &cfg(), CacheRepo(&fx.repo), &up, &meta(&fx, "9"))
            .await;
        assert_eq!(source, "rate-limited");
        assert!(started.elapsed() < Duration::from_millis(100), "at once");
        drop(held);
        for w in waiters {
            assert_eq!(w.await.unwrap(), "fetch");
        }
    }

    fn meta_for(up: &Upstream, version: &str) -> CargoArtifact {
        CargoArtifact::VersionMeta {
            api: up.base.clone(),
            name: "serde".into(),
            version: version.into(),
        }
    }

    #[tokio::test]
    async fn wait_past_gather_timeout_is_rate_limited() {
        let tuning = Tuning {
            gather_timeout: Duration::from_millis(300),
            ..fast()
        };
        let fx = Fx::new().await;
        let (engine, _writer) = engine_over(&fx, cfg(), tuning);
        let shared = engine.shared().clone();
        let host = fx.up.base.host_str().unwrap().to_string();
        let lane = shared.cargo_pacer.lane(&host, tuning.pacer_period);
        let holder = {
            let lane = lane.clone();
            tokio::spawn(async move {
                let _held = lane.gate.lock().await;
                tokio::time::sleep(tuning.gather_timeout * 3).await;
            })
        };
        tokio::task::yield_now().await;
        let mut up = fx.up.clone();
        up.dl_allow_private = true;
        let started = Instant::now();
        let permit = shared.inflight.clone().acquire_owned().await.unwrap();
        let (cached, source) = shared
            .cargo_pacer
            .fetch(&shared, &cfg(), CacheRepo(&fx.repo), &up, &meta(&fx, "1"))
            .await;
        drop(permit);
        assert!(cached.is_none());
        assert_eq!(source, "rate-limited");
        let waited = started.elapsed();
        assert!(
            waited >= tuning.gather_timeout && waited < tuning.gather_timeout * 2,
            "{waited:?}"
        );
        assert!(fx.hits().is_empty(), "no request of its own");
        assert_eq!(shared.inflight.available_permits(), crate::policy::INFLIGHT);
        holder.abort();
    }
}
