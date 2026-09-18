use bytes::Bytes;
use chrono::TimeDelta;

use super::*;
use crate::app::audit::Actor;
use crate::app::repositories::{CreateRepository, DeleteRepository};
use crate::domain::{Format, RepoKind, RepoSpec, Visibility};
use crate::ports::reclaim::Pinned;
use crate::testing::fakes::FakeDb;
use crate::testing::storage::MemStorage;

struct Fx {
    store: FakeDb,
    storage: MemStorage,
}

impl Fx {
    fn new() -> Self {
        Self {
            store: FakeDb::new(),
            storage: MemStorage::new(),
        }
    }

    fn reclaimer(&self, act_on_scan: bool) -> ReclaimOrphans {
        ReclaimOrphans::new(
            self.store.reclaim(),
            self.store.referenced(),
            Arc::new(self.storage.clone()),
            ReclaimPolicy {
                act_on_scan,
                ..ReclaimPolicy::default()
            },
        )
    }

    async fn put(&self, key: &str) {
        self.storage.put(key, Bytes::from_static(b"x")).await.unwrap();
    }

    async fn hosted(&self, name: &str) -> String {
        let repo = CreateRepository::new(
            self.store.repositories(),
            self.store.audit(),
            crate::server::event_bus(),
        )
        .guarding(Arc::new(self.storage.clone()))
        .run(&spec(name), &by(), Utc::now())
        .await
        .unwrap();
        let incarnation = self.store.repositories().incarnation(repo.id).await.unwrap().unwrap();
        layout::incarnation_prefix(&incarnation)
    }

    fn delete(&self) -> DeleteRepository {
        DeleteRepository::new(
            self.store.repositories(),
            self.store.audit(),
            crate::server::event_bus(),
            Arc::new(self.reclaimer(false)),
        )
    }
}

fn spec(name: &str) -> RepoSpec<'_> {
    RepoSpec {
        name,
        kind: RepoKind::Hosted,
        format: Format::Npm,
        visibility: Visibility::Public,
        upstream: None,
        members: &[],
    }
}

fn by() -> Actor<'static> {
    Actor {
        user_id: Some(1),
        username: "root",
        admin: true,
    }
}

fn later(hours: i64) -> DateTime<Utc> {
    Utc::now() + TimeDelta::hours(hours)
}

#[tokio::test]
async fn crashed_placer_pin_is_pruned_and_its_generation_reclaimed() {
    let fx = Fx::new();
    let prefix = fx.hosted("r").await;
    let logical = vec![format!("{prefix}/p/f")];
    let Pinned::Tokens(tokens) = fx
        .store
        .reclaim()
        .pin(&prefix, &logical, Utc::now() + TimeDelta::minutes(10))
        .await
        .unwrap()
    else {
        panic!("live prefix");
    };
    let generation = tokens[0].physical_key.clone();
    fx.put(&generation).await;

    let quiet = fx.reclaimer(false).run(later(1)).await;
    assert_eq!(quiet.scan_orphans, 0, "inside the grace, and pinned");

    let reported = fx.reclaimer(false).run(later(3)).await;
    assert_eq!(reported.pruned_pins, 1, "the dead placer's pin goes");
    assert_eq!(reported.scan_orphans, 1, "its generation is a scan candidate");
    assert!(fx.storage.contains(&generation), "reported, not acted on, by default");

    fx.reclaimer(true).run(later(3)).await;
    let acted = fx.reclaimer(true).run(later(3)).await;
    assert_eq!(acted.reclaimed, 1);
    assert!(!fx.storage.contains(&generation));
    assert!(fx.store.candidates().is_empty());
}

#[tokio::test]
async fn a_queued_orphan_is_deleted_once_due() {
    let fx = Fx::new();
    fx.put("npm/r/p/orphan.tgz").await;
    let now = Utc::now();
    fx.store
        .reclaim()
        .enqueue(&["npm/r/p/orphan.tgz".to_string()], now)
        .await
        .unwrap();
    assert_eq!(fx.reclaimer(false).run(now).await.reclaimed, 0, "not due inside the grace");
    assert!(fx.storage.contains("npm/r/p/orphan.tgz"));
    let report = fx.reclaimer(false).run(later(3)).await;
    assert_eq!(report.reclaimed, 1);
    assert_eq!(fx.storage.deleted(), vec!["npm/r/p/orphan.tgz".to_string()]);
}

#[tokio::test]
async fn a_failed_delete_leaves_the_candidate_for_the_next_pass() {
    let fx = Fx::new();
    fx.put("k/v").await;
    fx.store
        .reclaim()
        .enqueue(&["k/v".to_string()], Utc::now())
        .await
        .unwrap();
    fx.storage.fail_next("delete");
    let first = fx.reclaimer(false).run(later(3)).await;
    assert_eq!(first.failed, 1);
    assert!(fx.storage.contains("k/v"));
    let second = fx.reclaimer(false).run(later(5)).await;
    assert_eq!(second.reclaimed, 1, "the claim expired and was taken again");
}

#[tokio::test]
async fn retiring_a_repository_reclaims_its_prefixes_and_frees_its_name() {
    let fx = Fx::new();
    let prefix = fx.hosted("r").await;
    fx.put(&format!("{prefix}/p/f~g1")).await;
    fx.put("npm/r/legacy/p.tgz").await;
    fx.put("npm/r2/kept.tgz").await;

    fx.delete().run("r", &by(), Utc::now()).await.unwrap();

    assert_eq!(fx.storage.keys(), vec!["npm/r2/kept.tgz".to_string()]);
    let again = fx.hosted("r").await;
    assert_ne!(again, prefix, "a recreated name is a new incarnation");
}

#[tokio::test]
async fn recreation_refused_while_legacy_prefix_non_empty() {
    let fx = Fx::new();
    fx.hosted("r").await;
    fx.storage.fail_next("delete_batch");
    fx.put("npm/r/legacy/p.tgz").await;
    fx.delete().run("r", &by(), Utc::now()).await.unwrap();
    assert!(fx.storage.contains("npm/r/legacy/p.tgz"), "the failed batch left it");

    let refused = CreateRepository::new(
        fx.store.repositories(),
        fx.store.audit(),
        crate::server::event_bus(),
    )
    .guarding(Arc::new(fx.storage.clone()))
    .run(&spec("r"), &by(), Utc::now())
    .await
    .unwrap_err();
    let message = refused.to_string();
    assert!(message.contains("storage reclaim --prefix npm/r"), "{message}");

    fx.reclaimer(false).run(later(1)).await;
    assert!(
        !fx.storage.contains("npm/r/legacy/p.tgz"),
        "the sweep finishes the prefix once the dead claim expired"
    );
    fx.hosted("r").await;
}
