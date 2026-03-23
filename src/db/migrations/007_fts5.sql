CREATE VIRTUAL TABLE IF NOT EXISTS packages_fts USING fts5(
    name,
    description,
    content=packages,
    content_rowid=id
);

-- Triggers to keep FTS in sync
CREATE TRIGGER IF NOT EXISTS packages_fts_insert AFTER INSERT ON packages BEGIN
    INSERT INTO packages_fts(rowid, name, description) VALUES (new.id, new.name, new.description);
END;

CREATE TRIGGER IF NOT EXISTS packages_fts_delete AFTER DELETE ON packages BEGIN
    INSERT INTO packages_fts(packages_fts, rowid, name, description) VALUES('delete', old.id, old.name, old.description);
END;

CREATE TRIGGER IF NOT EXISTS packages_fts_update AFTER UPDATE ON packages BEGIN
    INSERT INTO packages_fts(packages_fts, rowid, name, description) VALUES('delete', old.id, old.name, old.description);
    INSERT INTO packages_fts(rowid, name, description) VALUES (new.id, new.name, new.description);
END;
