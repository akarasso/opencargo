//! The `PolicyStore` / `AuditStore` / `DependencyStore` / `VulnStore` /
//! `OciStore` contract, run twice: against the in-memory fake the unit tests use and
//! against the SQLite adapter the server runs on.
//!
//! The fake keeps full precision and the adapter truncates to the second,
//! which is why every timestamp clause states its tolerance.

mod common;

use chrono::Utc;
use common::contract::{cascade_contract, sqlite_with_repository, Release, TailHandles, TailPorts};
use common::fakes::FakeDb;
use opencargo::domain::{Format, RepoKind, RepoSpec, Visibility};
use opencargo::ports::packages::{NameMatch, NewRelease};
use tempfile::TempDir;

/// The published version the graph and the scan record hang off, stated
/// through `PackageStore` so the schema's foreign keys are satisfied by a
/// real release rather than by an invented id.
fn release(repository: i64) -> NewRelease<'static> {
    NewRelease {
        repository,
        package: "widget",
        match_name: NameMatch::Exact,
        description: None,
        readme: None,
        version: "1.0.0",
        metadata_json: "{}",
        checksum_sha1: None,
        checksum_sha256: None,
        integrity: None,
        size: 1,
        tarball_path: "npm/p/widget/widget-1.0.0.tgz",
        dist_tags: &[],
        dependencies: &[],
        pins: &[],
        now: Utc::now(),
    }
}

async fn fake() -> TailHandles {
    let db = FakeDb::new();
    db.repositories()
        .ensure_seeded(
            &[RepoSpec {
                name: "p",
                kind: RepoKind::Hosted,
                format: Format::Npm,
                visibility: Visibility::Public,
                upstream: None,
                members: &[],
            }],
            Utc::now(),
        )
        .await
        .unwrap();
    let repository = db.repositories().by_name("p").await.unwrap().unwrap().id;
    let landed = db
        .packages()
        .publish_version(&release(repository))
        .await
        .unwrap();
    TailHandles::new(
        TailPorts {
            policy: db.policy(),
            audit: db.audit(),
            deps: db.dependencies(),
            vulns: db.vulns(),
            oci: db.oci(),
            repository,
            release: Release {
                package: landed.package.id,
                version: landed.version.id,
            },
        },
        Box::new(db),
    )
}

async fn sqlite() -> TailHandles {
    let tmp = TempDir::new().unwrap();
    let stores = sqlite_with_repository(&tmp.path().join("contract.db")).await;
    let repository = stores
        .repositories()
        .by_name("p")
        .await
        .unwrap()
        .unwrap()
        .id;
    let landed = stores
        .packages()
        .publish_version(&release(repository))
        .await
        .unwrap();
    TailHandles::new(
        TailPorts {
            policy: stores.policy(),
            audit: stores.audit(),
            deps: stores.dependencies(),
            vulns: stores.vulns(),
            oci: stores.oci(),
            repository,
            release: Release {
                package: landed.package.id,
                version: landed.version.id,
            },
        },
        Box::new((tmp, stores)),
    )
}

cascade_contract!(fake_db, fake);
cascade_contract!(sqlite_adapter, sqlite);
