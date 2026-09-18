-- Aggregate download counter: one row per version instead of one row per
-- download. The legacy `downloads` table grew unbounded and serialized every
-- download behind SQLite's single writer. The `downloads` table is kept (no
-- longer written to) so the backfill below stays available and nothing breaks.
CREATE TABLE IF NOT EXISTS download_counts (
    version_id INTEGER PRIMARY KEY REFERENCES versions(id),
    count INTEGER NOT NULL DEFAULT 0
);

-- Backfill from the legacy per-row table (idempotent: PRIMARY KEY + OR IGNORE).
INSERT OR IGNORE INTO download_counts (version_id, count)
    SELECT version_id, COUNT(*) FROM downloads GROUP BY version_id;
