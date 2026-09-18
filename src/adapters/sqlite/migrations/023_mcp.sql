CREATE TABLE IF NOT EXISTS mcp_server_versions (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    repository_id INTEGER NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
    name TEXT NOT NULL,
    version TEXT NOT NULL,
    hosted INTEGER NOT NULL DEFAULT 0,
    response_json TEXT NOT NULL,
    schema_url TEXT,
    status TEXT NOT NULL DEFAULT 'active' CHECK(status IN ('active', 'deprecated', 'deleted')),
    status_message TEXT,
    status_changed_at TEXT,
    is_latest INTEGER NOT NULL DEFAULT 0,
    package_transports TEXT NOT NULL DEFAULT '',
    remote_transports TEXT NOT NULL DEFAULT '',
    remote_urls TEXT NOT NULL DEFAULT '',
    published_at TEXT,
    upstream_updated_at TEXT,
    synced_at TEXT NOT NULL,
    row_changed_at TEXT NOT NULL,
    permissions_sha256 TEXT NOT NULL,
    current_surface_id INTEGER REFERENCES mcp_surfaces(id) ON DELETE SET NULL,
    findings_high INTEGER NOT NULL DEFAULT 0,
    findings_medium INTEGER NOT NULL DEFAULT 0,
    UNIQUE (repository_id, name, version)
);

CREATE TABLE IF NOT EXISTS mcp_surfaces (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    version_id INTEGER NOT NULL REFERENCES mcp_server_versions(id) ON DELETE CASCADE,
    source TEXT NOT NULL CHECK(source IN ('declared', 'probe', 'attested')),
    remote_url TEXT NOT NULL DEFAULT '',
    remote_ordinal INTEGER NOT NULL DEFAULT 0,
    tools_json TEXT,
    tools_sha256 TEXT,
    permissions_sha256 TEXT NOT NULL,
    combined_sha256 TEXT NOT NULL,
    captured_at TEXT NOT NULL,
    captured_by TEXT,
    UNIQUE (version_id, source, remote_url, combined_sha256)
);

CREATE TABLE IF NOT EXISTS mcp_version_verdicts (
    repository_id INTEGER NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
    version_id INTEGER NOT NULL REFERENCES mcp_server_versions(id) ON DELETE CASCADE,
    surface_endpoints INTEGER NOT NULL DEFAULT 0,
    approved_endpoints INTEGER NOT NULL DEFAULT 0,
    worst_drift TEXT NOT NULL DEFAULT 'none'
        CHECK(worst_drift IN ('none', 'permissions', 'tools', 'both', 'new_endpoint')),
    drifted_remote TEXT,
    PRIMARY KEY (repository_id, version_id)
);

CREATE TABLE IF NOT EXISTS mcp_probe_runs (
    id INTEGER PRIMARY KEY,
    version_id INTEGER NOT NULL REFERENCES mcp_server_versions(id) ON DELETE CASCADE,
    remote_url TEXT NOT NULL,
    ran_at TEXT NOT NULL,
    ok INTEGER NOT NULL,
    protocol_version TEXT,
    error TEXT,
    surface_id INTEGER REFERENCES mcp_surfaces(id) ON DELETE SET NULL
);

CREATE TABLE IF NOT EXISTS mcp_findings (
    id INTEGER PRIMARY KEY,
    subject_kind TEXT NOT NULL CHECK(subject_kind IN ('surface', 'skill')),
    subject_id INTEGER NOT NULL,
    pattern TEXT NOT NULL,
    confidence TEXT NOT NULL CHECK(confidence IN ('medium', 'high')),
    promoted_by TEXT,
    field TEXT NOT NULL,
    tool TEXT NOT NULL DEFAULT '',
    span_start INTEGER NOT NULL,
    span_end INTEGER NOT NULL,
    excerpt TEXT NOT NULL,
    UNIQUE (subject_kind, subject_id, pattern, field, tool, span_start)
);

CREATE TABLE IF NOT EXISTS mcp_approvals (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    repository_id INTEGER NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
    subject_kind TEXT NOT NULL CHECK(subject_kind IN ('server', 'skill')),
    name TEXT NOT NULL,
    version TEXT NOT NULL,
    permissions_sha256 TEXT NOT NULL,
    tools_sha256 TEXT,
    combined_sha256 TEXT NOT NULL,
    remote_url TEXT NOT NULL DEFAULT '',
    surface_id INTEGER REFERENCES mcp_surfaces(id) ON DELETE SET NULL,
    state TEXT NOT NULL CHECK(state IN ('approved', 'blocked')),
    decided_by TEXT NOT NULL,
    decided_at TEXT NOT NULL,
    note TEXT,
    UNIQUE (repository_id, subject_kind, name, version, remote_url)
);

CREATE TABLE IF NOT EXISTS mcp_allow_rules (
    id INTEGER PRIMARY KEY,
    repository_id INTEGER NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
    pattern TEXT NOT NULL,
    effect TEXT NOT NULL CHECK(effect IN ('allow', 'deny')),
    created_by TEXT,
    created_at TEXT NOT NULL,
    UNIQUE (repository_id, pattern)
);

CREATE TABLE IF NOT EXISTS mcp_suppressions (
    id INTEGER PRIMARY KEY,
    repository_id INTEGER NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
    pattern TEXT NOT NULL,
    tool TEXT NOT NULL DEFAULT '',
    created_by TEXT,
    created_at TEXT NOT NULL,
    UNIQUE (repository_id, pattern, tool)
);

CREATE TABLE IF NOT EXISTS mcp_finding_counts (
    repository_id INTEGER NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
    version_id INTEGER NOT NULL REFERENCES mcp_server_versions(id) ON DELETE CASCADE,
    high INTEGER NOT NULL DEFAULT 0,
    medium INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (repository_id, version_id)
);

CREATE TABLE IF NOT EXISTS mcp_sync_state (
    repository_id INTEGER PRIMARY KEY REFERENCES repositories(id) ON DELETE CASCADE,
    high_water TEXT,
    last_full_at TEXT,
    last_run_at TEXT,
    last_error TEXT,
    skipped INTEGER NOT NULL DEFAULT 0,
    consecutive_failures INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE IF NOT EXISTS mcp_skills (
    id INTEGER PRIMARY KEY,
    repository_id INTEGER NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
    name TEXT NOT NULL,
    version TEXT NOT NULL,
    storage_key TEXT NOT NULL,
    sha256 TEXT NOT NULL,
    size INTEGER NOT NULL,
    description TEXT,
    allowed_tools TEXT,
    surface_sha256 TEXT NOT NULL,
    findings_high INTEGER NOT NULL DEFAULT 0,
    findings_medium INTEGER NOT NULL DEFAULT 0,
    blocking_findings INTEGER NOT NULL DEFAULT 0,
    published_by TEXT,
    published_at TEXT NOT NULL,
    UNIQUE (repository_id, name, version)
);

CREATE INDEX IF NOT EXISTS idx_mcp_ver_changed ON mcp_server_versions (repository_id, row_changed_at);
CREATE INDEX IF NOT EXISTS idx_mcp_ver_latest ON mcp_server_versions (repository_id, is_latest, name);
CREATE INDEX IF NOT EXISTS idx_mcp_find_subject ON mcp_findings (subject_kind, subject_id, confidence);
CREATE INDEX IF NOT EXISTS idx_mcp_probe_runs ON mcp_probe_runs (version_id, ran_at DESC);
CREATE INDEX IF NOT EXISTS idx_mcp_surf_current ON mcp_surfaces (version_id, source, remote_ordinal, captured_at DESC);
CREATE INDEX IF NOT EXISTS idx_mcp_approvals_subject ON mcp_approvals (subject_kind, name, version);
