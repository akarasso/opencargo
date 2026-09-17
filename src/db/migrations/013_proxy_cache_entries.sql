CREATE TABLE IF NOT EXISTS proxy_cache_entries (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    repository_id INTEGER NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
    kind TEXT NOT NULL,
    cache_key TEXT NOT NULL,
    status INTEGER NOT NULL,
    storage_path TEXT,
    content_type TEXT,
    etag TEXT,
    digest TEXT,
    size INTEGER NOT NULL DEFAULT 0,
    fetched_at TEXT NOT NULL DEFAULT (datetime('now')),
    expires_at TEXT,
    last_used_at TEXT NOT NULL DEFAULT (datetime('now')),
    UNIQUE(repository_id, kind, cache_key)
);
CREATE INDEX IF NOT EXISTS idx_proxy_cache_entries_expires ON proxy_cache_entries(expires_at);
CREATE INDEX IF NOT EXISTS idx_proxy_cache_entries_last_used ON proxy_cache_entries(last_used_at);
