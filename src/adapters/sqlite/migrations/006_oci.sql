CREATE TABLE IF NOT EXISTS oci_blobs (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    repository_id INTEGER NOT NULL REFERENCES repositories(id),
    digest TEXT NOT NULL,
    size INTEGER NOT NULL,
    content_type TEXT,
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    UNIQUE(repository_id, digest)
);

CREATE TABLE IF NOT EXISTS oci_manifests (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    repository_id INTEGER NOT NULL REFERENCES repositories(id),
    name TEXT NOT NULL,
    digest TEXT NOT NULL,
    content_type TEXT NOT NULL,
    size INTEGER NOT NULL,
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    UNIQUE(repository_id, name, digest)
);

CREATE TABLE IF NOT EXISTS oci_tags (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    repository_id INTEGER NOT NULL REFERENCES repositories(id),
    name TEXT NOT NULL,
    tag TEXT NOT NULL,
    manifest_digest TEXT NOT NULL,
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    UNIQUE(repository_id, name, tag)
);

CREATE TABLE IF NOT EXISTS oci_uploads (
    id TEXT PRIMARY KEY,
    repository_id INTEGER NOT NULL REFERENCES repositories(id),
    name TEXT NOT NULL,
    started_at TEXT NOT NULL DEFAULT (datetime('now'))
);
