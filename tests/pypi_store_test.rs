//! Port 15 (`PypiFileStore`), run against the fake and the SQLite adapter,
//! with the keys it references seen by ports 22 and 23.

mod common;

use common::contract::{pypi_contract, PypiHandles};
use common::fakes::FakeDb;
use opencargo::adapters::sqlite::SqliteStores;
use tempfile::TempDir;

async fn fake() -> PypiHandles {
    let db = FakeDb::new();
    PypiHandles::new(db.repositories(), db.pypi(), db.reclaim(), db.referenced(), Box::new(db))
}

async fn sqlite() -> PypiHandles {
    let tmp = TempDir::new().unwrap();
    let stores = SqliteStores::open(&tmp.path().join("pypi.db")).await.unwrap();
    PypiHandles::new(
        stores.repositories(),
        stores.pypi(),
        stores.reclaim(),
        stores.referenced(),
        Box::new((tmp, stores)),
    )
}

pypi_contract!(fake_db, fake);
pypi_contract!(sqlite_adapter, sqlite);
