CREATE TABLE IF NOT EXISTS server_secrets (
    name TEXT PRIMARY KEY,
    value BLOB NOT NULL,
    created_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS server_leases (
    name TEXT PRIMARY KEY,
    owner TEXT NOT NULL,
    version TEXT NOT NULL,
    acquired_at TEXT NOT NULL,
    renewed_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS server_state (
    name TEXT PRIMARY KEY,
    value TEXT NOT NULL,
    updated_at TEXT NOT NULL
);
