//! Port 24 (`RawFileStore`), run against the fake and the SQLite adapter,
//! with the keys it references seen by ports 22 and 23.

mod common;

use common::contract::{raw_contract, RawHandles};
use common::fakes::FakeDb;
use opencargo::adapters::sqlite::SqliteStores;
use tempfile::TempDir;

async fn fake() -> RawHandles {
    let db = FakeDb::new();
    RawHandles::new(db.repositories(), db.raw(), db.reclaim(), db.referenced(), Box::new(db))
}

async fn sqlite() -> RawHandles {
    let tmp = TempDir::new().unwrap();
    let stores = SqliteStores::open(&tmp.path().join("raw.db")).await.unwrap();
    RawHandles::new(
        stores.repositories(),
        stores.raw(),
        stores.reclaim(),
        stores.referenced(),
        Box::new((tmp, stores)),
    )
}

raw_contract!(fake_db, fake);
raw_contract!(sqlite_adapter, sqlite);
