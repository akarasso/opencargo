# Artifact storage

opencargo keeps metadata in SQLite and artifact bytes in one store: a
directory on disk (the default) or an S3-compatible bucket. The design is
`docs/design/ports-and-adapters.md` and the S3 v5 design it amends; this page
is the operator's view.

S3 is validated against MinIO in CI (`scripts/test-s3.sh`, the whole suite
plus real `docker`, `cargo`, `go` and `pnpm` clients). It has not yet been
validated from a second machine against a hosted provider; until it is, the
README does not list it and it should be treated as a preview.

## Choosing a backend

```toml
[storage]
backend = "s3"          # "fs" (default) or "s3"
id = "artifacts"        # declared identity; defaults to the role name

[storage.s3]
bucket = "opencargo"
region = "eu-west-3"
endpoint = "https://s3.example.net"   # omit for AWS
prefix = "prod"                        # isolates an instance inside a shared bucket
allow_http = false
virtual_hosted_style = false
request_timeout = "30s"                # how long the wire may stay idle
completion_timeout = "15m"             # outer bound of a completion or a copy
part_size_mib = 16
max_multipart_uploads = 8
exists_cache_entries = 10000           # 0 disables the positive existence cache
```

With `backend = "fs"` the store is `[server].storage_path`.

Credentials are never read from the file. The adapter reads only these
variables: `OPENCARGO_S3_ACCESS_KEY_ID` (or `AWS_ACCESS_KEY_ID`),
`OPENCARGO_S3_SECRET_ACCESS_KEY` (or `AWS_SECRET_ACCESS_KEY`),
`OPENCARGO_S3_SESSION_TOKEN` (or `AWS_SESSION_TOKEN`), `OPENCARGO_S3_REGION`
(or `AWS_REGION`), and `OPENCARGO_S3_ENDPOINT`, `OPENCARGO_S3_BUCKET`,
`OPENCARGO_S3_PREFIX`, which override the file. No profile file and no
instance metadata are consulted. TLS trusts the compiled-in Mozilla roots
only: an endpoint behind a private CA is not supported yet.

`opencargo validate-config <file>` refuses a missing bucket, a prefix with
empty, `.`, `..` or `_`-leading segments, a part size under 5 MiB and two
stores declaring the same identity.

## What the store holds

Every new key lives under the repository's incarnation, `r/<incarnation>/…`,
an opaque id allocated when the repository is created and never reused, so a
removed repository and a new one of the same name share no byte. Hosted
artifacts and OCI blobs and manifests are content-addressed and carry a
generation suffix (`~<id>`). Keys written before this layout (`npm/<repo>/…`,
`oci/<repo>/…`, `_proxy_cache/<repo>/…`) keep serving; nothing is moved.

Deletes are deferred. Unpublishing, deleting a manifest or a blob, evicting a
proxy entry or removing a repository enqueues the keys it released; the
always-on storage sweep (hourly, never at boot) reclaims queued keys after a
two-hour grace, under a claim that re-checks no row references them. Orphans
found by scanning the store are reported, not deleted. Disk held by orphans is
bounded by the rate they are produced times grace plus one sweep period.

OCI upload sessions idle for a day, and sessions started before this version,
are reaped by the same sweep.

## Commands

The server must be stopped for `migrate` and `reclaim`: they hold the
writer lease for their whole run and refuse while a server holds it; a server
starting meanwhile waits for them ([operations.md](operations.md)).

- `opencargo storage check`: the readiness probe, then every operation the
  backend exercises on its own reserved tree, cleaned up. Run it before
  switching a deployment to a new provider.
- `opencargo storage verify [--orphans]`: keys rows reference with no object,
  and with `--orphans` objects older than the grace that nothing references.
  Exits non-zero when a key is missing.
- `opencargo storage migrate --to <config.toml> [--dry-run]`: copies every
  object into the store the other config file declares, under the same keys.
  The target must not overlap the source. A rerun skips an object only when
  its key names the sha256 of its bytes and the target's bytes hash to it;
  every other object is copied again.
- `opencargo storage reclaim [--prefix P]`: one reclamation pass now, or the
  deletion of a prefix no repository names, for instance a legacy prefix
  that blocks the recreation of a repository name.

`GET /api/v1/system/storage` (admin) reports the backend kind, its declared
identity, readiness, the multipart uploads open and the reclamation backlog;
the System page shows it. No route names the endpoint, bucket or prefix.

## Operating S3

- Add a lifecycle rule aborting incomplete multipart uploads after a few
  days: the ledger sweep covers uploads of a crashed writer, the rule covers
  a lost database.
- A warm hit costs a `HEAD` and a `GET`; the positive existence cache saves
  the first while one process writes the prefix. Disable it before running
  several instances against one prefix.
- A cold proxy transfer is stored before it is served and read back to
  serve it; outside R2 the egress is billed twice.
- The largest OCI blob is 5 GiB, the single-copy ceiling of CopyObject.
- Running the suite on S3: `make test-s3` starts a pinned MinIO in Docker and
  runs `cargo test` with `OPENCARGO_TEST_STORAGE=s3`.
