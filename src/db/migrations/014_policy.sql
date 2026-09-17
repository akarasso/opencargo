CREATE TABLE IF NOT EXISTS policy_resolutions (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    requested_repo TEXT NOT NULL,
    member_repo TEXT NOT NULL,
    format TEXT NOT NULL,
    name TEXT NOT NULL,
    version TEXT,
    digest TEXT,
    published_at TEXT,
    date_source TEXT NOT NULL DEFAULT '',
    actor TEXT NOT NULL,
    actor_kind TEXT NOT NULL CHECK (actor_kind IN ('token', 'user', 'static', 'anonymous')),
    user_id INTEGER,
    would_block INTEGER NOT NULL DEFAULT 0,
    unknown INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE IF NOT EXISTS policy_verdicts (
    resolution_id INTEGER NOT NULL REFERENCES policy_resolutions(id) ON DELETE CASCADE,
    rule TEXT NOT NULL,
    verdict TEXT NOT NULL CHECK (verdict IN ('pass', 'would_block', 'unknown', 'not_applicable')),
    reason TEXT NOT NULL DEFAULT '',
    PRIMARY KEY (resolution_id, rule)
);

CREATE INDEX IF NOT EXISTS idx_policy_res_created ON policy_resolutions(created_at);
CREATE INDEX IF NOT EXISTS idx_policy_res_user ON policy_resolutions(user_id, created_at);
CREATE INDEX IF NOT EXISTS idx_policy_res_repo_created ON policy_resolutions(requested_repo, created_at);
CREATE INDEX IF NOT EXISTS idx_policy_res_member_name ON policy_resolutions(member_repo, name);
CREATE INDEX IF NOT EXISTS idx_policy_verdicts_rule ON policy_verdicts(rule, verdict);
