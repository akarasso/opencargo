CREATE TABLE IF NOT EXISTS server_secrets (
    name TEXT PRIMARY KEY,
    value BLOB NOT NULL,
    created_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS sso_identities (
    provider TEXT NOT NULL,
    issuer TEXT NOT NULL,
    subject TEXT NOT NULL,
    user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    email TEXT,
    provisioned INTEGER NOT NULL,
    role_before_link TEXT,
    disabled INTEGER NOT NULL DEFAULT 0,
    linked_at TEXT NOT NULL,
    last_login_at TEXT NOT NULL,
    PRIMARY KEY (provider, issuer, subject)
);

CREATE INDEX IF NOT EXISTS idx_sso_identities_user ON sso_identities(user_id);

CREATE TABLE IF NOT EXISTS sso_user_states (
    user_id INTEGER PRIMARY KEY REFERENCES users(id) ON DELETE CASCADE,
    disabled_by TEXT NOT NULL,
    disabled_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS sso_token_provenance (
    token_id TEXT PRIMARY KEY REFERENCES api_tokens(id) ON DELETE CASCADE,
    provider TEXT NOT NULL,
    issuer TEXT NOT NULL,
    subject TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_sso_token_provenance_identity
    ON sso_token_provenance(provider, issuer, subject);

CREATE TABLE IF NOT EXISTS sso_outages (
    provider TEXT NOT NULL,
    issuer TEXT NOT NULL,
    started_at TEXT NOT NULL,
    ended_at TEXT,
    PRIMARY KEY (provider, issuer, started_at)
);

CREATE TABLE IF NOT EXISTS login_handoffs (
    code_hash TEXT PRIMARY KEY,
    binding TEXT NOT NULL,
    payload TEXT NOT NULL,
    expires_at TEXT NOT NULL,
    consumed_at TEXT
);

CREATE INDEX IF NOT EXISTS idx_login_handoffs_expires ON login_handoffs(expires_at);
