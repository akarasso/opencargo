CREATE TABLE IF NOT EXISTS raw_files (
    id INTEGER PRIMARY KEY,
    repository_id INTEGER NOT NULL REFERENCES repositories(id),
    path TEXT NOT NULL,
    physical_key TEXT NOT NULL,
    size INTEGER NOT NULL,
    sha256 TEXT NOT NULL,
    content_type TEXT,
    uploaded_by TEXT NOT NULL,
    uploaded_at TEXT NOT NULL,
    UNIQUE(repository_id, path)
);

CREATE INDEX IF NOT EXISTS idx_raw_files_key ON raw_files(physical_key);
