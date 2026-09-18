CREATE TABLE IF NOT EXISTS maven_values (
    id INTEGER PRIMARY KEY,
    repository_id INTEGER NOT NULL REFERENCES repositories(id),
    ga TEXT NOT NULL,
    version TEXT NOT NULL,
    versioned INTEGER NOT NULL DEFAULT 0,
    UNIQUE(repository_id, ga, version)
);

CREATE TABLE IF NOT EXISTS maven_units (
    id INTEGER PRIMARY KEY,
    value_id INTEGER NOT NULL REFERENCES maven_values(id) ON DELETE CASCADE,
    build TEXT NOT NULL,
    depositor TEXT NOT NULL,
    contested INTEGER NOT NULL DEFAULT 0,
    refused INTEGER NOT NULL DEFAULT 0,
    visible_at TEXT,
    created_at TEXT NOT NULL,
    revision INTEGER NOT NULL,
    UNIQUE(value_id, build)
);

CREATE TABLE IF NOT EXISTS maven_files (
    id INTEGER PRIMARY KEY,
    unit_id INTEGER NOT NULL REFERENCES maven_units(id) ON DELETE CASCADE,
    filename TEXT NOT NULL,
    physical_key TEXT NOT NULL,
    size INTEGER NOT NULL,
    sha1 TEXT NOT NULL,
    md5 TEXT NOT NULL,
    sha256 TEXT NOT NULL,
    sha512 TEXT NOT NULL,
    depositor TEXT NOT NULL,
    created_at TEXT NOT NULL,
    UNIQUE(unit_id, filename)
);

CREATE TABLE IF NOT EXISTS maven_declarations (
    unit_id INTEGER NOT NULL REFERENCES maven_units(id) ON DELETE CASCADE,
    filename TEXT NOT NULL,
    algorithm TEXT NOT NULL,
    value TEXT NOT NULL,
    created_at TEXT NOT NULL,
    PRIMARY KEY (unit_id, filename, algorithm)
);

CREATE TABLE IF NOT EXISTS maven_client_metadata (
    repository_id INTEGER NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
    dir TEXT NOT NULL,
    sha1 TEXT NOT NULL,
    md5 TEXT NOT NULL,
    sha256 TEXT NOT NULL,
    sha512 TEXT NOT NULL,
    release TEXT,
    latest TEXT,
    plugins TEXT,
    updated_at TEXT NOT NULL,
    PRIMARY KEY (repository_id, dir)
);

CREATE TABLE IF NOT EXISTS maven_counters (
    repository_id INTEGER NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
    scope TEXT NOT NULL,
    counter INTEGER NOT NULL,
    updated_at TEXT NOT NULL,
    PRIMARY KEY (repository_id, scope)
);

CREATE INDEX IF NOT EXISTS idx_maven_files_key ON maven_files(physical_key);

CREATE INDEX IF NOT EXISTS idx_maven_units_pending ON maven_units(visible_at, created_at);
