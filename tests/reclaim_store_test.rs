//! Ports 22 and 23 (`ReclaimStore`, `ReferencedKeys`), run against the fake
//! and the SQLite adapter, with the enqueues `retire` and `delete_version`
//! write in their own transactions.

mod common;

use common::contract::{reclaim_contract, ReclaimHandles};
use common::fakes::FakeDb;
use opencargo::adapters::sqlite::SqliteStores;
use tempfile::TempDir;

async fn fake() -> ReclaimHandles {
    let db = FakeDb::new();
    ReclaimHandles::new(
        db.repositories(),
        db.packages(),
        db.oci(),
        db.reclaim(),
        db.referenced(),
        Box::new(db),
    )
}

async fn sqlite() -> ReclaimHandles {
    let tmp = TempDir::new().unwrap();
    let stores = SqliteStores::open(&tmp.path().join("reclaim.db"))
        .await
        .unwrap();
    ReclaimHandles::new(
        stores.repositories(),
        stores.packages(),
        stores.oci(),
        stores.reclaim(),
        stores.referenced(),
        Box::new((tmp, stores)),
    )
}

reclaim_contract!(fake_db, fake);
reclaim_contract!(sqlite_adapter, sqlite);
