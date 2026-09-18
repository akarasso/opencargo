-- The multipart ledger's table: one row per server-side upload this cluster
-- has open, so an upload whose writer died with its process still has
-- something the sweep can abort.
--
-- Neither timestamp carries a DEFAULT. Every statement of the adapter binds
-- the caller's clock, which is what makes "a row holds the time the caller
-- passed, never the server's" an assertion instead of a hope.
CREATE TABLE IF NOT EXISTS storage_multipart (
    upload_id TEXT PRIMARY KEY,
    object_key TEXT NOT NULL,
    started_at TEXT NOT NULL,
    touched_at TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_storage_multipart_touched ON storage_multipart(touched_at);
