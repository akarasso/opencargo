use bytes::Bytes;

use super::*;
use crate::testing::fakes::FakeDb;
use crate::testing::storage::MemStorage;

fn fixture() -> (FakeDb, MemStorage, HighWaterMark) {
    let db = FakeDb::new();
    let storage = MemStorage::new();
    let mark = HighWaterMark::new(db.reclaim(), Arc::new(storage.clone()));
    (db, storage, mark)
}

async fn put(storage: &MemStorage, value: &Mark) {
    storage
        .put(layout::MARK, serde_json::to_vec(value).unwrap().into())
        .await
        .unwrap();
}

#[tokio::test]
async fn a_fresh_installation_is_level_and_writes_the_mark_ahead() {
    let (db, _storage, mark) = fixture();
    let state = db.reclaim().epoch().await.unwrap();
    assert_eq!(mark.compare(&state).await.unwrap(), Standing::Level);
    assert!(mark.permit().await.unwrap());
    let written = mark.read().await.unwrap().unwrap();
    assert_eq!(written.counter, 1);
    assert_eq!(written.installation, state.installation);
    assert_eq!(db.reclaim().epoch().await.unwrap().counter, 1);
}

#[tokio::test]
async fn a_database_behind_the_mark_draws_a_new_epoch_and_owes_a_verify() {
    let (db, storage, mark) = fixture();
    assert!(mark.permit().await.unwrap());
    let state = db.reclaim().epoch().await.unwrap();
    // The mark is written before the counter: a crash between the two.
    put(
        &storage,
        &Mark {
            installation: state.installation.clone(),
            epoch: state.epoch.clone(),
            counter: state.counter + 1,
        },
    )
    .await;
    assert_eq!(mark.compare(&state).await.unwrap(), Standing::DatabaseBehind);
    assert_eq!(mark.guard().await.unwrap(), Standing::DatabaseBehind);
    let after = db.reclaim().epoch().await.unwrap();
    assert_ne!(after.epoch, state.epoch, "a fresh epoch, never a counter");
    assert!(after.verify_pending, "reclamation is refused until the verify");
    assert!(!mark.permit().await.unwrap());
}

#[tokio::test]
async fn a_storage_behind_the_database_verifies_under_the_same_epoch() {
    let (db, storage, mark) = fixture();
    assert!(mark.permit().await.unwrap());
    let state = db.reclaim().epoch().await.unwrap();
    storage.delete(layout::MARK).await.unwrap();
    assert_eq!(mark.compare(&state).await.unwrap(), Standing::StorageBehind);
    assert_eq!(mark.guard().await.unwrap(), Standing::StorageBehind);
    let after = db.reclaim().epoch().await.unwrap();
    assert_eq!(after.epoch, state.epoch, "a storage behind draws no epoch");
    assert!(after.verify_pending);

    db.reclaim().verified(&after.epoch).await.unwrap();
    put(
        &storage,
        &Mark {
            installation: state.installation.clone(),
            epoch: state.epoch.clone(),
            counter: 0,
        },
    )
    .await;
    assert_eq!(mark.guard().await.unwrap(), Standing::StorageBehind);
    assert_eq!(db.reclaim().epoch().await.unwrap().epoch, state.epoch);
}

#[tokio::test]
async fn another_installations_mark_is_a_hard_refusal() {
    let (db, storage, mark) = fixture();
    let state = db.reclaim().epoch().await.unwrap();
    put(
        &storage,
        &Mark {
            installation: "someone else".to_string(),
            epoch: state.epoch.clone(),
            counter: 1,
        },
    )
    .await;
    assert!(matches!(mark.guard().await, Err(MarkError::Foreign(_))));
    assert!(mark.permit().await.is_err());
}

#[tokio::test]
async fn an_unreadable_mark_refuses_reclamation_and_nothing_else() {
    let (_db, storage, mark) = fixture();
    storage.put(layout::MARK, Bytes::from_static(b"{")).await.unwrap();
    assert!(matches!(mark.read().await, Err(MarkError::Unreadable)));
    assert!(mark.permit().await.is_err());
}
