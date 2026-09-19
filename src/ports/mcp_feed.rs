//! The two outbound conversations MCP governance holds: paging an upstream
//! registry's catalog, and asking a remote server for its tools.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde_json::Value;

/// One page request of `/v0.1/servers`. `include_deleted` is always sent
/// explicitly: a takedown must reach a mirror whatever the upstream's
/// default.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FeedQuery {
    pub cursor: Option<String>,
    pub updated_since: Option<DateTime<Utc>>,
    pub limit: u32,
    pub include_deleted: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct FeedPage {
    pub servers: Vec<Value>,
    pub next_cursor: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum FeedError {
    #[error("upstream answered {0}")]
    Status(u16),
    #[error("upstream unreachable: {0}")]
    Transport(String),
    #[error("upstream answered something unreadable: {0}")]
    Invalid(String),
}

#[async_trait]
pub trait RegistryFeed: Send + Sync {
    async fn page(&self, base: &url::Url, query: &FeedQuery) -> Result<FeedPage, FeedError>;
}

/// Who may be probed: an admin grants loopback and private addresses per
/// repository, and a probe header when one is configured.
#[derive(Debug, Clone, Default)]
pub struct ProbeOptions {
    pub allow_private: bool,
    pub auth_header: Option<(String, String)>,
}

/// What a remote server answered to `tools/list`.
#[derive(Debug, Clone, PartialEq)]
pub struct ProbeAnswer {
    pub tools: Vec<Value>,
    pub protocol_version: String,
}

#[async_trait]
pub trait ToolProbe: Send + Sync {
    /// `Err` is the reason a run failed, recorded as it is.
    async fn tools(&self, url: &str, options: &ProbeOptions) -> Result<ProbeAnswer, String>;
}
