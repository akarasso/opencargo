CREATE TABLE IF NOT EXISTS proxy_cache_meta (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    repository_id INTEGER NOT NULL REFERENCES repositories(id),
    cache_key TEXT NOT NULL,
    fetched_at TEXT NOT NULL DEFAULT (datetime('now')),
    ttl_seconds INTEGER NOT NULL DEFAULT 86400,
    UNIQUE(repository_id, cache_key)
);
