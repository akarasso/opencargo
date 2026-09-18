//! The `WebhookStore` contract, run twice: against the in-memory fake the
//! unit tests build on, and against the SQLite adapter the server runs on.

mod common;

use common::contract::{store_contract, Handles, Ports};
use common::fakes::FakeDb;
use opencargo::adapters::sqlite::SqliteStores;
use tempfile::TempDir;

async fn fake() -> Handles {
    let db = FakeDb::new();
    let ports = Ports {
        webhooks: db.webhooks(),
        repos: db.repositories(),
        packages: db.packages(),
        search: db.search(),
        deps: db.dependencies(),
    };
    Handles::new(ports, Box::new(db))
}

async fn sqlite() -> Handles {
    let tmp = TempDir::new().unwrap();
    let stores = SqliteStores::open(&tmp.path().join("contract.db"))
        .await
        .unwrap();
    let ports = Ports {
        webhooks: stores.webhooks(),
        repos: stores.repositories(),
        packages: stores.packages(),
        search: stores.search(),
        deps: stores.dependencies(),
    };
    Handles::new(ports, Box::new((tmp, stores)))
}

store_contract!(fake_db, fake);
store_contract!(sqlite_adapter, sqlite);
