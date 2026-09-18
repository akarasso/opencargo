//! The smallest adapters: the machine's clock, its id generator, and the
//! secrets one process keeps.

use std::collections::HashMap;
use std::sync::Mutex;

use async_trait::async_trait;
use chrono::{DateTime, Utc};

use crate::error::StoreError;
use crate::ports::clock::Clock;
use crate::ports::ids::Ids;
use crate::ports::secrets::ServerSecretStore;

pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> DateTime<Utc> {
        Utc::now()
    }
}

pub struct UuidIds;

impl Ids for UuidIds {
    fn token_id(&self) -> String {
        uuid::Uuid::new_v4().to_string()
    }

    fn upload_id(&self) -> String {
        uuid::Uuid::new_v4().to_string()
    }
}

/// Secrets that live as long as the process: today's behaviour, where a
/// restart invalidates every registry token. The persisted store lands with
/// the first of migrations 021/022, which creates `server_secrets`.
#[derive(Default)]
pub struct ProcessSecrets {
    secrets: Mutex<HashMap<String, Vec<u8>>>,
}

#[async_trait]
impl ServerSecretStore for ProcessSecrets {
    async fn get_or_init(&self, name: &str, candidate: &[u8]) -> Result<Vec<u8>, StoreError> {
        let mut secrets = self.secrets.lock().unwrap_or_else(|e| e.into_inner());
        Ok(secrets
            .entry(name.to_string())
            .or_insert_with(|| candidate.to_vec())
            .clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[tokio::test]
    async fn concurrent_first_calls_agree_on_one_secret() {
        let store = Arc::new(ProcessSecrets::default());
        let calls = (0u8..8).map(|i| {
            let store = store.clone();
            tokio::spawn(async move { store.get_or_init("k", &[i; 32]).await.unwrap() })
        });
        let mut seen = Vec::new();
        for call in calls {
            seen.push(call.await.unwrap());
        }
        assert!(seen.windows(2).all(|w| w[0] == w[1]));
        assert_eq!(store.get_or_init("k", &[99; 32]).await.unwrap(), seen[0]);
    }
}
