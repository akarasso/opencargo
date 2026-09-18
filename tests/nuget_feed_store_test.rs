//! Port 17 `NugetFeedRead`, run against the fake and the SQLite adapter.

mod common;

use common::contract::{nuget_feed_contract, FeedHandles};
use common::fakes::FakeDb;
use opencargo::adapters::sqlite::SqliteStores;
use tempfile::TempDir;

async fn fake() -> FeedHandles {
    let db = FakeDb::new();
    FeedHandles::new(db.repositories(), db.packages(), db.nuget_feed(), Box::new(db))
}

async fn sqlite() -> FeedHandles {
    let tmp = TempDir::new().unwrap();
    let stores = SqliteStores::open(&tmp.path().join("feed.db")).await.unwrap();
    FeedHandles::new(
        stores.repositories(),
        stores.packages(),
        stores.nuget_feed(),
        Box::new((tmp, stores)),
    )
}

nuget_feed_contract!(fake_db, fake);
nuget_feed_contract!(sqlite_adapter, sqlite);
