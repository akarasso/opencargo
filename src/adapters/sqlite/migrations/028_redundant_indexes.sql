-- Two indexes a unique constraint already carries: UNIQUE(package_id, version)
-- and UNIQUE(package_id, tag) answer every lookup by package that these were
-- kept for, and both were rewritten by every publish.
DROP INDEX IF EXISTS idx_versions_package;

DROP INDEX IF EXISTS idx_dist_tags_package;
