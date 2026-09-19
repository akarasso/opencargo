# Operating one instance

opencargo runs as **one instance per database**. SQLite has one writer and
must stay on one host; a second process on the same database refuses to start.
Read scale is federation: other opencargo instances proxying this one (see
[`k8s/sidecar/`](../k8s/sidecar/)); each keeps its own cache, upstream tokens
and crates.io pacer, so N readers make N times the outbound requests.

## The writer lease

At startup the server takes a lease row in its database before running any
migration, renews it every `lease_renew`, and gives it back on a clean
shutdown. Another process that finds a live lease waits up to `lease_wait`,
then exits naming the holder's id, version and last renewal. A lease left by a
killed process goes stale after `lease_stale_after`, so the restart takes it
over. `opencargo migrate`, `storage migrate` and `storage reclaim` take the
same lease; `migrate --force` skips it for a holder you know is dead.

The lease is a guard, not fencing: nothing on a request path reads it, and it
is only meaningful on one host. `/data` on NFS is not supported. `lease = false`
is for tests; two instances on one database are not supported, and the System
page shows `lease disabled`. A lease lost while serving is not a readiness
failure: the server keeps serving, logs a warning, shows `lease lost` on the
System page, and stops running the background sweeps and scheduled backups
until it takes the lease back. The fix is to stop the other process.

```toml
[server]
lease = true
lease_wait = "60s"         # longer than lease_stale_after
lease_stale_after = "30s"  # at least three lease_renew
lease_renew = "10s"
shutdown_grace = "30s"     # in-flight requests after the drain starts
endpoint_drain = "0s"      # one poll period behind an ingress or mesh that polls its targets
```

`OPENCARGO_LEASE_WAIT`, `OPENCARGO_SHUTDOWN_GRACE` and
`OPENCARGO_ENDPOINT_DRAIN` override the file (a bare number is seconds). The
Helm chart renders them from `lease.waitSeconds` and `shutdown.*`, so change
them in `values.yaml`, not in the `config` block: that is what keeps
`terminationGracePeriodSeconds` in step with the process.

## Shutdown and the upgrade window

On SIGTERM: `/health/ready` answers `503 draining` for `endpoint_drain` while
everything still serves, WebSocket clients get a close frame (1001), then the
listener closes and in-flight requests finish within `shutdown_grace`. A
download longer than that is cut. The chart's `terminationGracePeriodSeconds`
is `preStop sleep + endpoint_drain + 5 s WebSocket close + shutdown_grace + 10`
(47 s at the defaults).

Deployments use `strategy: Recreate`: the old pod is gone before the new one
starts, so every upgrade has a window. Its terms:

- graceful: preStop (2 s) + endpoint_drain + WebSocket close (up to 5 s) +
  in-flight drain (up to shutdown_grace) + pod recreate + the volume ownership
  pass + one startup period (5 s) + one readiness period (10 s);
- after a kill (OOM, SIGKILL past the grace): lease_stale_after (30 s) + pod
  recreate + the volume ownership pass + the two probe periods.

The ownership pass is the kubelet's `fsGroup` walk plus, in `k8s/base`, a
`chown` of `/data/db` and `/data/storage`; it grows with the artifact tree. A
`[backup].to` under `/data` adds its snapshots to that walk, a `to` outside it
does not. No measured figures are published yet.

## Publish limits

Publishing is counted per account in a sliding window. A refusal is a `429`
carrying `Retry-After` and the limit it hit:

```
{"error":"publish rate limit reached for npm: 30 per 60s, retry in 41s"}
```

Out of the box that is thirty npm publishes and thirty PyPI uploads a minute
per account, and nothing else. Cargo, Go, NuGet, MCP (a `server.json` or a
skill archive) and raw publishes are metered only once you configure them. An
OCI push is several requests: its blob uploads are not counted, its manifest
put -- the one request that makes the image exist -- is, so the count is an
image count and a multi-platform image counts each platform's manifest and its
index. A Maven deploy is a file per request with none that completes it, so it
is never metered here, and a `[limits.publish.format]` entry for `maven` is
refused at startup rather than accepted as a setting that does nothing.

One limit applies to a publish -- the most specific one configured:

| entry | applies to |
|---|---|
| `[limits.publish.repository]` `<repo>` | every publish into that repository |
| `[limits.publish.format]` `<format>` | every publish of that format elsewhere |
| `[limits.publish]` `per_window` | every format with no entry of its own, and it drops the two shipped defaults |

A repository entry replaces its format's rather than adding to it, and each
entry counts in a window of its own: an account publishing into two
repositories that both carry an entry has each allowance separately.

### A CI account that publishes a batch

Give the repository CI publishes into its own allowance and leave the rest of
the server where it is:

```toml
[limits.publish.repository]
npm-ci = { max = 2000, per = "1h" }
```

A limit is finite by construction: `0`, and a window over 24h, are refused at
startup with every other problem in the config. Raise a limit rather than
lift it -- there is no value that turns the meter off, because on the formats
it counts the meter is what keeps a stolen token from flooding the store. What
it does not count, and what a stolen token can still fill the store with: OCI
blobs uploaded ahead of any manifest, and Maven files.

## Backups

```toml
[backup]
enabled = false
every = "24h"   # divides the day; runs at t ≡ at (mod every), UTC
at = "03:00"
keep = 7
to = "/backups"
storage = false # the scheduler copies the database only

[backup.sink]   # optional off-box copy of every finished snapshot
backend = "s3"
id = "backup"
[backup.sink.s3]
bucket = "registry-dr"
prefix = "snapshots"
```

`opencargo backup --to <dir> [--storage] [--keep N] [--force]` writes
`<dir>/opencargo-<time>/`: `db.sqlite` (a `VACUUM INTO` copy of the live
database), with `--storage` every object under `storage/` and its digest in
`keys.sha256`, and `manifest.json` last. The database is copied before the
objects, so a publish during the run leaves an orphan object, never a row
without one. The schedule runs the same code on the lease holder, first at the
next `at`, never at boot; a missed window is skipped.

- A snapshot with `storage: false` is a recovery point for the database only,
  not a disaster recovery: restoring it onto an empty volume leaves rows with
  no object, so `restore` refuses it without `--force`.
- Two runs into one `to` serialise on `<to>/.backup.lock`; a run reclaims the
  snapshots an interrupted run left there (no manifest). The Instance tile
  counts them until then.
- A run needs `(keep + 1) × (database + storage) + peak WAL` on `to`: pruning
  happens after the new snapshot is written. It checks free space first and
  refuses rather than fills the volume. A sink does not reduce this: the copy
  is staged locally.
- `[backup.sink]` must not overlap `[storage]`: another endpoint, region or
  bucket, or a prefix that is not the other's segment. The run compares the
  two resolved stores before uploading, so an environment override that
  collapses them fails the run. The sink is built by the run, so an
  unreachable sink fails the backup, not the boot. Sink uploads interrupted
  mid-way are left to the bucket's lifecycle rules.
- `opencargo backup --check <dir>` verifies the manifest version, the database
  digest and integrity, and every object against `keys.sha256`.

## Restore drill

A restore never runs inside the serving pod: the server holds `{db}.lock`, and
`restore` needs it exclusively.

1. `kubectl scale deploy/opencargo --replicas=0` and wait for the pod to go.
2. Set `restore.enabled=true` and `restore.from=<snapshot>` (Helm) or edit
   `k8s/restore-job.yaml`, and apply the Job.
3. Read its log to the end: it verifies the snapshot, restores the objects,
   swaps the database in and prints the `storage verify` command to run.
4. Scale back to 1 and run that command.

If the Job stopped half-way, `{db}.restore-in-progress` remains and `serve`,
`migrate` and `backup` refuse to start, `--force` included. Re-apply the same
Job: the restore of the snapshot the marker names resumes. A restore from
another snapshot is refused while the marker exists; deleting it by hand is
unsupported. `{db}.lock` is permanent and carries the lock; leave it.

## After a rollback

A restore draws a fresh restore epoch, and the server owes a `storage verify`
before it reclaims anything again. The same applies to a database put back by
any other route — a file copied over, a snapshot adopted by hand: the
high-water mark under `_opencargo/` in the artifact store is compared at
startup and before every sweep, and a database behind it draws a fresh epoch
by itself. Until that verify has run, nothing is deleted.

`opencargo storage verify` lists the keys rows reference with no object
behind them; there is no repair without a store that keeps noncurrent
versions, so on a plain filesystem the loss is reported and the version,
manifest or file has to be deleted and published again. On a store that keeps
them, `opencargo storage verify --repair` puts the last noncurrent version of
each of those keys back. `--orphans` lists what no row references; after a
rollback those that lie under a live repository are queued for reclamation,
which claims and re-checks before it deletes.

## Continuous replication

Litestream (pinned `v0.5.17`) can replicate the database off the volume; it is
the operator's choice, not something opencargo ships. It replicates the
database only: pair it with an object-storage `[storage]` backend or a storage
sync, or a restore brings back rows whose objects were never copied.

opencargo needs no setting for it, and that is a measured answer rather than an
assumption: a drill of 7 985 publishes at `sync-interval: 1s`, killed with
`kill -9`, restored every publish the server had acknowledged — including the
ones acknowledged in the last second — and the `-wal` rose to a 6.1 MiB working
set in the first thirty seconds and stayed there for the rest of the run.
A write-ahead log that checkpoints is reused in place and never truncated, so a
flat plateau is the healthy shape; the failure to watch for is a file still
climbing when the process dies, which is what a replicator holding a read lock
would produce.

```yaml
# litestream.yml, as a sidecar beside the server
dbs:
  - path: /data/db/opencargo.db
    replicas:
      - type: s3
        bucket: registry-dr
        path: opencargo
        sync-interval: 1s
```

Restoring: stop the server first — `restore` needs `{db}.lock` exclusively, and
restoring under a live instance is the one way to get two writers on two
diverging databases sharing one artifact store. `litestream restore` produces a
bare database file, while `opencargo restore --from <dir>` expects a snapshot
directory, so the file is wrapped in one:

```sh
litestream restore -o /tmp/restored/db.sqlite /data/db/opencargo.db
sha=$(sha256sum /tmp/restored/db.sqlite | cut -d' ' -f1)
cat > /tmp/restored/manifest.json <<JSON
{"version":1,"taken_at":"$(date -u +%Y-%m-%dT%H:%M:%SZ)","db_sha256":"$sha",
 "storage":false,"storage_keys":0,"storage_bytes":0}
JSON
opencargo restore --from /tmp/restored --force
```

`--force` is what accepts a database-only snapshot. Taking this door rather than
copying the file into place is what draws a fresh restore epoch and makes the
server owe a `storage verify` before it reclaims anything again.

Replaying the drill on your own instance: raise `[limits.publish].per_window`
first. The shipped limiter caps a synthetic load at thirty publishes a minute,
and a run that small never reaches the checkpoint window — the `-wal` then looks
flat for a reason that has nothing to do with replication.
