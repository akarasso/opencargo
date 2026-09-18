-- Track which blobs each manifest references. The manifest JSON lists its
-- config blob + layers, but there was no queryable link, so blob deletion
-- could break a live image and deleting a manifest leaked its blobs.
-- This table lets us (a) refuse deleting a still-referenced blob and (b) GC
-- orphaned blobs when a manifest is deleted.
-- NB: only populated for manifests pushed after this migration; manifests that
-- predate it are not retro-linked (acceptable: a fresh push re-links them).
CREATE TABLE IF NOT EXISTS oci_manifest_blobs (
    repository_id INTEGER NOT NULL REFERENCES repositories(id),
    manifest_digest TEXT NOT NULL,
    blob_digest TEXT NOT NULL,
    UNIQUE(repository_id, manifest_digest, blob_digest)
);

CREATE INDEX IF NOT EXISTS idx_oci_manifest_blobs_blob
    ON oci_manifest_blobs(repository_id, blob_digest);
