//! Port 18 (`MavenFileStore`), run against the fake and the SQLite adapter,
//! with the reclamation ports it fences with and contributes to.

mod common;

use common::contract::{maven_contract, MavenHandles};
use common::fakes::FakeDb;
use opencargo::adapters::sqlite::SqliteStores;
use tempfile::TempDir;

async fn fake() -> MavenHandles {
    let db = FakeDb::new();
    MavenHandles::new(
        db.repositories(),
        db.maven(),
        db.reclaim(),
        db.referenced(),
        Box::new(db),
    )
}

async fn sqlite() -> MavenHandles {
    let tmp = TempDir::new().unwrap();
    let stores = SqliteStores::open(&tmp.path().join("maven.db"))
        .await
        .unwrap();
    MavenHandles::new(
        stores.repositories(),
        stores.maven(),
        stores.reclaim(),
        stores.referenced(),
        Box::new((tmp, stores)),
    )
}

maven_contract!(fake_db, fake);
maven_contract!(sqlite_adapter, sqlite);
