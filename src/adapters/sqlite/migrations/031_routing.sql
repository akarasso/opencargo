CREATE TABLE IF NOT EXISTS routing_rules (
    name TEXT PRIMARY KEY,
    format TEXT NOT NULL,
    patterns TEXT NOT NULL,
    except_idents TEXT NOT NULL,
    effect TEXT NOT NULL,
    targets TEXT NOT NULL,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS routing_snapshot_version (
    id INTEGER PRIMARY KEY,
    version INTEGER NOT NULL
);

INSERT OR IGNORE INTO routing_snapshot_version (id, version) VALUES (1, 1);

CREATE INDEX IF NOT EXISTS idx_routing_rules_format ON routing_rules(format);
