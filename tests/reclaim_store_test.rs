//! Ports 22 and 23 (`ReclaimStore`, `ReferencedKeys`), run against the fake
//! and the SQLite adapter, with the enqueues `retire` and `delete_version`
//! write in their own transactions.

mod common;

use common::contract::{reclaim_contract, ReclaimHandles, Referencing};
use common::fakes::FakeDb;
use opencargo::adapters::sqlite::SqliteStores;
use tempfile::TempDir;

async fn fake() -> ReclaimHandles {
    let db = FakeDb::new();
    ReclaimHandles::new(
        db.repositories(),
        Referencing {
            packages: db.packages(),
            oci: db.oci(),
            maven: db.maven(),
        },
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
        Referencing {
            packages: stores.packages(),
            oci: stores.oci(),
            maven: stores.maven(),
        },
        stores.reclaim(),
        stores.referenced(),
        Box::new((tmp, stores)),
    )
}

reclaim_contract!(fake_db, fake);
reclaim_contract!(sqlite_adapter, sqlite);
