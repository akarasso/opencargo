CREATE TABLE IF NOT EXISTS pypi_files (
    id INTEGER PRIMARY KEY,
    repository_id INTEGER NOT NULL REFERENCES repositories(id),
    package_id INTEGER NOT NULL REFERENCES packages(id),
    version_id INTEGER NOT NULL REFERENCES versions(id),
    filename TEXT NOT NULL,
    packagetype TEXT NOT NULL,
    sha256 TEXT NOT NULL,
    size INTEGER NOT NULL,
    storage_key TEXT NOT NULL,
    metadata_key TEXT,
    metadata_sha256 TEXT,
    requires_python TEXT,
    yanked INTEGER NOT NULL DEFAULT 0,
    yanked_reason TEXT,
    uploaded_at TEXT NOT NULL,
    UNIQUE(repository_id, filename)
);

CREATE INDEX IF NOT EXISTS idx_pypi_files_version ON pypi_files(version_id);

CREATE INDEX IF NOT EXISTS idx_pypi_files_package ON pypi_files(package_id);
