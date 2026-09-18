//! The `WebhookStore` contract, run twice: against the in-memory fake the
//! unit tests build on, and against the SQLite adapter the server runs on.

mod common;

use common::contract::{store_contract, Handles};
use common::fakes::FakeDb;
use opencargo::adapters::sqlite::SqliteStores;
use tempfile::TempDir;

async fn fake() -> Handles {
    let db = FakeDb::new();
    Handles::new(db.webhooks(), Box::new(db))
}

async fn sqlite() -> Handles {
    let tmp = TempDir::new().unwrap();
    let stores = SqliteStores::open(&tmp.path().join("contract.db"))
        .await
        .unwrap();
    Handles::new(stores.webhooks(), Box::new((tmp, stores)))
}

store_contract!(fake_db, fake);
store_contract!(sqlite_adapter, sqlite);
