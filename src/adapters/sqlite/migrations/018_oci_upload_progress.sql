ALTER TABLE oci_blobs ADD COLUMN storage_key TEXT;

UPDATE oci_blobs SET storage_key = (
    SELECT 'oci/' || r.name || '/_blobs/sha256/' || replace(oci_blobs.digest, 'sha256:', '')
    FROM repositories r WHERE r.id = oci_blobs.repository_id
) WHERE storage_key IS NULL;

ALTER TABLE oci_manifests ADD COLUMN storage_key TEXT;

UPDATE oci_manifests SET storage_key = (
    SELECT 'oci/' || r.name || '/' || oci_manifests.name || '/manifests/' || oci_manifests.name
           || '/sha256/' || replace(oci_manifests.digest, 'sha256:', '')
    FROM repositories r WHERE r.id = oci_manifests.repository_id
) WHERE storage_key IS NULL;

ALTER TABLE oci_uploads ADD COLUMN segment_prefix TEXT;

ALTER TABLE oci_uploads ADD COLUMN received INTEGER;

ALTER TABLE oci_uploads ADD COLUMN segment_count INTEGER;

ALTER TABLE oci_uploads ADD COLUMN touched_at TEXT;

ALTER TABLE oci_uploads ADD COLUMN lease_token TEXT;

ALTER TABLE oci_uploads ADD COLUMN lease_until TEXT;

CREATE TABLE IF NOT EXISTS oci_upload_segments (
    upload_id TEXT NOT NULL,
    start_at INTEGER NOT NULL,
    length INTEGER NOT NULL,
    storage_key TEXT NOT NULL,
    PRIMARY KEY (upload_id, start_at)
);

CREATE INDEX IF NOT EXISTS idx_oci_uploads_touched ON oci_uploads(touched_at);
