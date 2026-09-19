//! The `RepositoryStore` / `PackageStore` / `SearchIndex` contract, run
//! twice: against the in-memory fake the unit tests build on, and against the
//! SQLite adapter the server runs on.

mod common;

use common::contract::{cached_contract, package_contract, Handles, Ports};
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
        cached: db.cached_packages(),
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
        cached: stores.cached_packages(),
        deps: stores.dependencies(),
    };
    Handles::new(ports, Box::new((tmp, stores)))
}

package_contract!(fake_db, fake);
package_contract!(sqlite_adapter, sqlite);
cached_contract!(fake_cached, fake);
cached_contract!(sqlite_cached, sqlite);
