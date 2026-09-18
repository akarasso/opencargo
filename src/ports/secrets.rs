//! Port 21 (A1 C1): server-wide secrets, such as the key registry tokens are
//! signed with.

use async_trait::async_trait;

use crate::error::StoreError;

#[async_trait]
pub trait ServerSecretStore: Send + Sync {
    /// The secret stored under `name`, created from `candidate` when there is
    /// none. Concurrent first calls agree on one value.
    async fn get_or_init(&self, name: &str, candidate: &[u8]) -> Result<Vec<u8>, StoreError>;
}
