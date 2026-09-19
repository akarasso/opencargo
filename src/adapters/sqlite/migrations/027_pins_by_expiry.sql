-- The pin table is a queue ordered by expiry, so its key is its expiry: the
-- rebuilt table is its own primary-key index (WITHOUT ROWID) and its own
-- expiry index, where the old shape carried four b-trees for a row that a
-- publish inserts and deletes again seconds later.
BEGIN IMMEDIATE;

CREATE TABLE reclaim_pins_by_expiry (
    until TEXT NOT NULL,
    token TEXT NOT NULL,
    physical_key TEXT NOT NULL,
    repo_prefix TEXT NOT NULL,
    PRIMARY KEY (until, token)
) WITHOUT ROWID;

INSERT INTO reclaim_pins_by_expiry (until, token, physical_key, repo_prefix)
    SELECT until, token, physical_key, repo_prefix FROM reclaim_pins;

DROP TABLE reclaim_pins;

ALTER TABLE reclaim_pins_by_expiry RENAME TO reclaim_pins;

CREATE INDEX idx_reclaim_pins_physical ON reclaim_pins(physical_key);

COMMIT;
