//! The `ProxyCacheStore` contract, run twice: against the in-memory fake and
//! against the SQLite adapter the server runs on. The fake keeps full
//! precision and the adapter truncates to the second, which is why every
//! timestamp clause states its tolerance.

mod common;

use common::contract::{proxy_cache_contract, sqlite_with_repository, CacheHandles};
use common::fakes::FakeDb;
use tempfile::TempDir;

async fn fake() -> CacheHandles {
    let db = FakeDb::new();
    CacheHandles::new(db.proxy_cache(), Box::new(db))
}

async fn sqlite() -> CacheHandles {
    let tmp = TempDir::new().unwrap();
    let stores = sqlite_with_repository(&tmp.path().join("contract.db")).await;
    CacheHandles::new(stores.proxy_cache(), Box::new((tmp, stores)))
}

proxy_cache_contract!(fake_db, fake);
proxy_cache_contract!(sqlite_adapter, sqlite);
