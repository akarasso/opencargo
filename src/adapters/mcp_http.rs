//! The MCP outbound ports over HTTP: an upstream registry's catalog pages.

use std::time::Duration;

use async_trait::async_trait;
use serde_json::Value;

use crate::ports::mcp_feed::{FeedError, FeedPage, FeedQuery, RegistryFeed};

const PAGE_BYTES: usize = 16 << 20;
const PAGE_TIMEOUT: Duration = Duration::from_secs(60);

pub struct HttpRegistryFeed {
    client: reqwest::Client,
}

impl HttpRegistryFeed {
    pub fn new() -> anyhow::Result<Self> {
        let client = reqwest::Client::builder()
            .timeout(PAGE_TIMEOUT)
            .redirect(crate::proxy::redirect_policy())
            .user_agent(concat!("opencargo/", env!("CARGO_PKG_VERSION")))
            .build()?;
        Ok(Self { client })
    }
}

/// `{base}/v0.1/servers`, the base's own path kept.
pub fn servers_url(base: &url::Url, query: &FeedQuery) -> Result<url::Url, FeedError> {
    let mut url = base.clone();
    let path = format!("{}/v0.1/servers", base.path().trim_end_matches('/'));
    url.set_path(&path);
    url.set_query(None);
    {
        let mut pairs = url.query_pairs_mut();
        pairs.append_pair("limit", &query.limit.clamp(1, 100).to_string());
        if let Some(cursor) = &query.cursor {
            pairs.append_pair("cursor", cursor);
        }
        if let Some(since) = query.updated_since {
            pairs.append_pair("updated_since", &since.to_rfc3339_opts(chrono::SecondsFormat::Secs, true));
        }
        pairs.append_pair("include_deleted", if query.include_deleted { "true" } else { "false" });
    }
    Ok(url)
}

#[async_trait]
impl RegistryFeed for HttpRegistryFeed {
    async fn page(&self, base: &url::Url, query: &FeedQuery) -> Result<FeedPage, FeedError> {
        let url = servers_url(base, query)?;
        let resp = self
            .client
            .get(url.as_str())
            .header(reqwest::header::ACCEPT, "application/json")
            .send()
            .await
            .map_err(|e| FeedError::Transport(e.to_string()))?;
        if !resp.status().is_success() {
            return Err(FeedError::Status(resp.status().as_u16()));
        }
        if resp.content_length().is_some_and(|n| n as usize > PAGE_BYTES) {
            return Err(FeedError::Invalid("page too large".into()));
        }
        let bytes = resp.bytes().await.map_err(|e| FeedError::Transport(e.to_string()))?;
        if bytes.len() > PAGE_BYTES {
            return Err(FeedError::Invalid("page too large".into()));
        }
        let body: Value = serde_json::from_slice(&bytes).map_err(|e| FeedError::Invalid(e.to_string()))?;
        let servers = body
            .get("servers")
            .and_then(Value::as_array)
            .cloned()
            .ok_or_else(|| FeedError::Invalid("no servers array".into()))?;
        let next_cursor = body
            .get("metadata")
            .and_then(|m| m.get("nextCursor"))
            .and_then(Value::as_str)
            .filter(|c| !c.is_empty())
            .map(str::to_string);
        Ok(FeedPage { servers, next_cursor })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn limit_over_100_is_clamped_before_the_request_and_the_flag_is_explicit() {
        let base = url::Url::parse("https://registry.example/prefix/").unwrap();
        let url = servers_url(
            &base,
            &FeedQuery {
                cursor: Some("a/b:1.0.0".into()),
                updated_since: None,
                limit: 500,
                include_deleted: true,
            },
        )
        .unwrap();
        assert_eq!(url.path(), "/prefix/v0.1/servers");
        let pairs: Vec<(String, String)> = url.query_pairs().map(|(k, v)| (k.into(), v.into())).collect();
        assert!(pairs.contains(&("limit".into(), "100".into())));
        assert!(pairs.contains(&("cursor".into(), "a/b:1.0.0".into())));
        assert!(pairs.contains(&("include_deleted".into(), "true".into())));
    }
}
