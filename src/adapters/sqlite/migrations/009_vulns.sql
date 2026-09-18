CREATE TABLE IF NOT EXISTS vulnerability_scans (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    version_id INTEGER NOT NULL REFERENCES versions(id) ON DELETE CASCADE,
    scanned_at TEXT NOT NULL DEFAULT (datetime('now')),
    total_deps INTEGER NOT NULL DEFAULT 0,
    vulnerable_deps INTEGER NOT NULL DEFAULT 0,
    scan_results_json TEXT,
    status TEXT NOT NULL DEFAULT 'clean' CHECK(status IN ('clean', 'warning', 'critical'))
);

CREATE INDEX IF NOT EXISTS idx_vuln_scans_version ON vulnerability_scans(version_id);
