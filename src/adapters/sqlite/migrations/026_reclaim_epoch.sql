CREATE TABLE IF NOT EXISTS reclaim_epoch (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    installation TEXT NOT NULL,
    epoch TEXT NOT NULL,
    counter INTEGER NOT NULL,
    verify_pending INTEGER NOT NULL
);

INSERT OR IGNORE INTO reclaim_epoch (id, installation, epoch, counter, verify_pending)
    VALUES (1, lower(hex(randomblob(16))), lower(hex(randomblob(16))), 0, 0);
