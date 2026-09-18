CREATE TABLE IF NOT EXISTS repositories (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    name TEXT NOT NULL UNIQUE,
    repo_type TEXT NOT NULL CHECK(repo_type IN ('hosted', 'proxy', 'group')),
    format TEXT NOT NULL CHECK(format IN ('npm', 'cargo', 'oci', 'go', 'pypi')),
    visibility TEXT NOT NULL DEFAULT 'private' CHECK(visibility IN ('public', 'private')),
    upstream_url TEXT,
    config_json TEXT,
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    updated_at TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE TABLE IF NOT EXISTS packages (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    repository_id INTEGER NOT NULL REFERENCES repositories(id),
    name TEXT NOT NULL,
    description TEXT,
    readme TEXT,
    license TEXT,
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    updated_at TEXT NOT NULL DEFAULT (datetime('now')),
    UNIQUE(repository_id, name)
);

CREATE TABLE IF NOT EXISTS versions (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    package_id INTEGER NOT NULL REFERENCES packages(id),
    version TEXT NOT NULL,
    metadata_json TEXT NOT NULL,
    checksum_sha1 TEXT,
    checksum_sha256 TEXT,
    integrity TEXT,
    size INTEGER NOT NULL DEFAULT 0,
    tarball_path TEXT NOT NULL,
    published_at TEXT NOT NULL DEFAULT (datetime('now')),
    UNIQUE(package_id, version)
);

CREATE TABLE IF NOT EXISTS dist_tags (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    package_id INTEGER NOT NULL REFERENCES packages(id),
    tag TEXT NOT NULL,
    version_id INTEGER NOT NULL REFERENCES versions(id),
    UNIQUE(package_id, tag)
);

CREATE TABLE IF NOT EXISTS downloads (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    version_id INTEGER NOT NULL REFERENCES versions(id),
    downloaded_at TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE INDEX IF NOT EXISTS idx_packages_name ON packages(name);
CREATE INDEX IF NOT EXISTS idx_versions_package ON versions(package_id);
CREATE INDEX IF NOT EXISTS idx_dist_tags_package ON dist_tags(package_id);
CREATE INDEX IF NOT EXISTS idx_downloads_version ON downloads(version_id);
