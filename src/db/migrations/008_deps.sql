CREATE TABLE IF NOT EXISTS package_dependencies (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    package_id INTEGER NOT NULL REFERENCES packages(id) ON DELETE CASCADE,
    version_id INTEGER NOT NULL REFERENCES versions(id) ON DELETE CASCADE,
    dependency_name TEXT NOT NULL,
    dependency_version_req TEXT NOT NULL,
    dependency_type TEXT NOT NULL DEFAULT 'runtime',
    created_at TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE INDEX IF NOT EXISTS idx_package_deps_package ON package_dependencies(package_id);
CREATE INDEX IF NOT EXISTS idx_package_deps_dep_name ON package_dependencies(dependency_name);
CREATE INDEX IF NOT EXISTS idx_package_deps_version ON package_dependencies(version_id);
