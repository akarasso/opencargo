//! The multipart ledger in memory, for the storage adapters' tests.

use std::sync::Mutex;
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, TimeDelta, Utc};

use crate::error::StoreError;
use crate::ports::multipart::{Abandoned, MultipartLedger};

/// The ledger's contract in memory, keyed on the caller's clock.
#[derive(Default)]
pub struct MemLedger {
    rows: Mutex<Vec<(String, String, DateTime<Utc>)>>,
}

#[async_trait]
impl MultipartLedger for MemLedger {
    async fn opened(&self, upload_id: &str, key: &str, now: DateTime<Utc>) -> Result<(), StoreError> {
        self.rows.lock().unwrap().push((upload_id.to_string(), key.to_string(), now));
        Ok(())
    }

    async fn touched(&self, upload_id: &str, now: DateTime<Utc>) -> Result<(), StoreError> {
        for row in self.rows.lock().unwrap().iter_mut().filter(|r| r.0 == upload_id) {
            row.2 = now;
        }
        Ok(())
    }

    async fn closed(&self, upload_id: &str) -> Result<(), StoreError> {
        self.rows.lock().unwrap().retain(|r| r.0 != upload_id);
        Ok(())
    }

    async fn idle_since(&self, age: Duration, now: DateTime<Utc>) -> Result<Vec<Abandoned>, StoreError> {
        let cutoff = now - TimeDelta::from_std(age).unwrap();
        Ok(self
            .rows
            .lock()
            .unwrap()
            .iter()
            .filter(|r| r.2 < cutoff)
            .map(|r| (r.0.clone(), r.1.clone()))
            .collect())
    }

    async fn in_flight(&self) -> Result<u64, StoreError> {
        Ok(self.rows.lock().unwrap().len() as u64)
    }
}

