use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use serde_json::Value;
use sqlx::SqlitePool;
use tracing::{info, warn};

use crate::error::AppError;
use crate::storage::FilesystemStorage;
use crate::storage::StorageBackend;

/// An HTTP client that fetches from upstream registries and caches responses.
#[derive(Clone)]
pub struct ProxyClient {
    client: reqwest::Client,
    storage: Arc<FilesystemStorage>,
    db: SqlitePool,
}

impl ProxyClient {
    /// Create a new ProxyClient with the given timeout.
    pub fn new(
        storage: Arc<FilesystemStorage>,
        db: SqlitePool,
        connect_timeout_secs: u64,
    ) -> Self {
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(connect_timeout_secs))
            .timeout(Duration::from_secs(connect_timeout_secs * 3))
            .build()
            .expect("failed to build reqwest client");

        Self {
            client,
            storage,
            db,
        }
    }

    /// Fetch package metadata from an upstream registry, caching the JSON response.
    ///
    /// `repo_name` is used for the cache key namespace.
    /// `upstream_url` should be the base URL of the upstream registry (e.g. `https://registry.npmjs.org`).
    /// `package_name` is the full npm package name (e.g. `react` or `@scope/name`).
    pub async fn fetch_package_metadata(
        &self,
        repo_name: &str,
        upstream_url: &str,
        package_name: &str,
        repo_id: i64,
        ttl_seconds: u64,
    ) -> Result<Value, AppError> {
        let cache_key = format!("metadata:{}", package_name);
        let cache_storage_path = format!(
            "_proxy_cache/{}/{}/metadata.json",
            repo_name, package_name
        );

        // Check if cache is fresh
        let is_fresh = crate::db::is_proxy_cache_fresh(&self.db, repo_id, &cache_key).await;

        if is_fresh {
            // Try to return cached metadata
            if let Ok(data) = self.storage.get(&cache_storage_path).await {
                let cached: Value = serde_json::from_slice(&data)
                    .map_err(|e| AppError::Internal(format!("corrupt cached metadata: {e}")))?;
                info!(
                    package = %package_name,
                    repo = %repo_name,
                    "Serving package metadata from cache"
                );
                return Ok(cached);
            }
        }

        // Fetch from upstream
        let url = format!(
            "{}/{}",
            upstream_url.trim_end_matches('/'),
            package_name
        );

        match self.client.get(&url).send().await {
            Ok(response) => {
                if !response.status().is_success() {
                    // Upstream returned an error. If we have a cached version, serve it.
                    if let Ok(data) = self.storage.get(&cache_storage_path).await {
                        warn!(
                            package = %package_name,
                            status = %response.status(),
                            "Upstream returned error, serving stale cache"
                        );
                        let cached: Value = serde_json::from_slice(&data)
                            .map_err(|e| AppError::Internal(format!("corrupt cached metadata: {e}")))?;
                        return Ok(cached);
                    }
                    return Err(AppError::NotFound(format!(
                        "package not found upstream: {package_name} (status {})",
                        response.status()
                    )));
                }

                let body = response.bytes().await.map_err(|e| {
                    AppError::Internal(format!("failed to read upstream response: {e}"))
                })?;

                let metadata: Value = serde_json::from_slice(&body).map_err(|e| {
                    AppError::Internal(format!("invalid JSON from upstream: {e}"))
                })?;

                // Cache the metadata
                self.storage
                    .put(&cache_storage_path, Bytes::from(body.to_vec()))
                    .await?;

                // Update cache meta in DB
                let _ = crate::db::upsert_proxy_cache_meta(
                    &self.db,
                    repo_id,
                    &cache_key,
                    ttl_seconds as i64,
                )
                .await;

                info!(
                    package = %package_name,
                    repo = %repo_name,
                    "Fetched and cached package metadata from upstream"
                );

                Ok(metadata)
            }
            Err(e) => {
                // Network error. Try stale cache.
                if let Ok(data) = self.storage.get(&cache_storage_path).await {
                    warn!(
                        package = %package_name,
                        error = %e,
                        "Upstream unreachable, serving stale cache"
                    );
                    let cached: Value = serde_json::from_slice(&data)
                        .map_err(|err| AppError::Internal(format!("corrupt cached metadata: {err}")))?;
                    return Ok(cached);
                }
                Err(AppError::Internal(format!(
                    "upstream unreachable and no cache available: {e}"
                )))
            }
        }
    }

    /// Fetch a tarball from an upstream registry, caching it in storage.
    ///
    /// Tarballs are immutable by version so they are cached indefinitely once fetched.
    pub async fn fetch_tarball(
        &self,
        repo_name: &str,
        upstream_url: &str,
        package_name: &str,
        filename: &str,
        repo_id: i64,
    ) -> Result<Bytes, AppError> {
        let cache_storage_path = format!(
            "_proxy_cache/{}/{}/{}",
            repo_name, package_name, filename
        );

        // Check if already cached
        if self.storage.exists(&cache_storage_path).await? {
            info!(
                package = %package_name,
                filename = %filename,
                "Serving tarball from cache"
            );
            return self.storage.get(&cache_storage_path).await;
        }

        // Fetch from upstream
        let url = format!(
            "{}/{}/-/{}",
            upstream_url.trim_end_matches('/'),
            package_name,
            filename
        );

        let response = self.client.get(&url).send().await.map_err(|e| {
            AppError::Internal(format!("failed to fetch tarball from upstream: {e}"))
        })?;

        if !response.status().is_success() {
            return Err(AppError::NotFound(format!(
                "tarball not found upstream: {filename} (status {})",
                response.status()
            )));
        }

        let data = response.bytes().await.map_err(|e| {
            AppError::Internal(format!("failed to read upstream tarball: {e}"))
        })?;

        // Cache the tarball (infinite TTL for tarballs)
        self.storage
            .put(&cache_storage_path, data.clone())
            .await?;

        // Record cache meta in DB (very long TTL — effectively infinite)
        let _ = crate::db::upsert_proxy_cache_meta(
            &self.db,
            repo_id,
            &format!("tarball:{}/{}", package_name, filename),
            315_360_000, // ~10 years
        )
        .await;

        info!(
            package = %package_name,
            filename = %filename,
            size = data.len(),
            "Fetched and cached tarball from upstream"
        );

        Ok(data)
    }
}

/// Rewrite all `dist.tarball` URLs in an npm package metadata document
/// to point to our proxy server.
///
/// The upstream URLs are replaced with `{base_url}/{repo_name}/{package_name}/-/{filename}`.
pub fn rewrite_tarball_urls(
    metadata: &mut Value,
    base_url: &str,
    repo_name: &str,
    package_name: &str,
) {
    if let Some(versions) = metadata.get_mut("versions").and_then(|v| v.as_object_mut()) {
        for (_version_key, version_meta) in versions.iter_mut() {
            if let Some(dist) = version_meta.get_mut("dist").and_then(|d| d.as_object_mut()) {
                if let Some(tarball_url) = dist.get("tarball").and_then(|t| t.as_str()) {
                    // Extract the filename from the upstream tarball URL
                    if let Some(filename) = tarball_url.rsplit('/').next() {
                        let new_url = format!(
                            "{}/{}/{}/-/{}",
                            base_url.trim_end_matches('/'),
                            repo_name,
                            package_name,
                            filename
                        );
                        dist.insert("tarball".to_string(), Value::String(new_url));
                    }
                }
            }
        }
    }
}
