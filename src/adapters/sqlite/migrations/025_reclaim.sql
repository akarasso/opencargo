CREATE TABLE IF NOT EXISTS repository_incarnations (
    repository_id INTEGER PRIMARY KEY REFERENCES repositories(id) ON DELETE CASCADE,
    incarnation TEXT NOT NULL UNIQUE
);

INSERT OR IGNORE INTO repository_incarnations (repository_id, incarnation)
    SELECT id, lower(hex(randomblob(16))) FROM repositories;

CREATE TABLE IF NOT EXISTS storage_prefixes (
    prefix TEXT PRIMARY KEY,
    incarnation TEXT NOT NULL,
    legacy INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_storage_prefixes_incarnation ON storage_prefixes(incarnation);

INSERT OR IGNORE INTO storage_prefixes (prefix, incarnation, legacy)
    SELECT 'r/' || i.incarnation, i.incarnation, 0 FROM repository_incarnations i;

INSERT OR IGNORE INTO storage_prefixes (prefix, incarnation, legacy)
    SELECT r.format || '/' || r.name, i.incarnation, 1
    FROM repositories r JOIN repository_incarnations i ON i.repository_id = r.id;

INSERT OR IGNORE INTO storage_prefixes (prefix, incarnation, legacy)
    SELECT '_proxy_cache/' || r.name, i.incarnation, 1
    FROM repositories r JOIN repository_incarnations i ON i.repository_id = r.id;

CREATE TABLE IF NOT EXISTS retired_incarnations (
    incarnation TEXT PRIMARY KEY,
    retired_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS reclaim_pins (
    token TEXT PRIMARY KEY,
    physical_key TEXT NOT NULL,
    repo_prefix TEXT NOT NULL,
    until TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_reclaim_pins_key ON reclaim_pins(physical_key);

CREATE INDEX IF NOT EXISTS idx_reclaim_pins_until ON reclaim_pins(until);

CREATE TABLE IF NOT EXISTS reclaim_claimed (
    physical_key TEXT PRIMARY KEY,
    claimed_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS reclaim_claims (
    key TEXT PRIMARY KEY,
    token TEXT NOT NULL,
    until TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS reclaim_candidates (
    key TEXT PRIMARY KEY,
    prefix INTEGER NOT NULL,
    enqueued_at TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_reclaim_candidates_enqueued ON reclaim_candidates(enqueued_at);
