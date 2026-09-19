CREATE TABLE IF NOT EXISTS cached_packages (
    id INTEGER PRIMARY KEY,
    repository_id INTEGER NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
    format TEXT NOT NULL,
    name TEXT NOT NULL,
    description TEXT,
    latest_version TEXT,
    first_seen_at TEXT NOT NULL,
    last_seen_at TEXT NOT NULL,
    UNIQUE (repository_id, name)
);

CREATE INDEX IF NOT EXISTS idx_cached_packages_repo ON cached_packages(repository_id);

CREATE VIRTUAL TABLE IF NOT EXISTS cached_packages_fts USING fts5(
    name,
    description,
    content=cached_packages,
    content_rowid=id
);

CREATE TRIGGER IF NOT EXISTS cached_packages_fts_insert AFTER INSERT ON cached_packages BEGIN
    INSERT INTO cached_packages_fts(rowid, name, description) VALUES (new.id, new.name, new.description);
END;

CREATE TRIGGER IF NOT EXISTS cached_packages_fts_delete AFTER DELETE ON cached_packages BEGIN
    INSERT INTO cached_packages_fts(cached_packages_fts, rowid, name, description) VALUES('delete', old.id, old.name, old.description);
END;

CREATE TRIGGER IF NOT EXISTS cached_packages_fts_update AFTER UPDATE ON cached_packages BEGIN
    INSERT INTO cached_packages_fts(cached_packages_fts, rowid, name, description) VALUES('delete', old.id, old.name, old.description);
    INSERT INTO cached_packages_fts(rowid, name, description) VALUES (new.id, new.name, new.description);
END;
