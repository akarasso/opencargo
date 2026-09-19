# Performance

What one opencargo process costs, per workload. Everything on this page comes
from `scripts/bench.sh`, which boots a release binary on a scratch directory,
drives a scenario, samples the process through `/proc`, and writes
`results.json`, `results.csv` and `report.md` side by side. Nothing here is an
estimate: a figure that was not measured is marked as not measured.

```sh
make bench                       # every scenario, into bench-results/<utc>/
make bench-smoke                 # the harness itself, offline, a few seconds
scripts/bench.sh --scenarios idle,publish --publishes 100 --settle 30
```

## Method

Each scenario boots a server of its own, with its own port, its own SQLite
file, its own storage tree and therefore its own cold caches. A scenario emits
one or more *records*, and a record is one row of the table.

- **RSS.** Sampled from `/proc/<pid>/status` every 100 ms. The peak is
  `VmHWM`, which the harness resets through `/proc/<pid>/clear_refs` when a
  record opens, so a peak belongs to its record and not to the process's whole
  life. The steady figure is the median RSS over the settle window.
- **CPU.** `utime + stime` from `/proc/<pid>/stat`. The average is over the
  record's whole window, the peak is the highest 100 ms interval, and both are
  percentages of one core — 100% means one core saturated, not the machine.
- **Wall time.** The load phase only: fixtures and curl scripts are built
  before the clock starts, and summarising happens after it stops.
- **Latency.** One `curl` process drives a whole batch through libcurl's multi
  interface, one `next` block per request, so a p50 is a request and not a
  process spawn. The exception is the OCI push, whose upload session cannot be
  scripted ahead; its latency is a whole session and includes the client's
  process starts, and it is labelled as such in the notes.
- **Bytes written.** `write_bytes` from `/proc/<pid>/io`, after `sync`, so it
  includes the WAL and the server's own log — `log_bytes` in the JSON says how
  much of it was the log. `storage_bytes` is the apparent size of the
  artifacts where they landed — the store directory, or MinIO's bucket when
  the row ran on S3 — and `db_bytes` the SQLite file plus its `-wal` and
  `-shm`, since a publish still in the write-ahead log would otherwise read as
  a database that never grew.
- **Steady state vs peak.** After the load, the server is left alone for the
  settle window (`--settle`, 15 s by default) and sampled throughout. Steady
  figures come from that window, peaks from the whole record.

Readiness is a deadline and a probe, never a sleep: the harness waits for the
server's own `Listening on 127.0.0.1:<port>` line *and* a 200 from
`/health/ready` before it starts. The port is drawn below the ephemeral range,
because a port taken from inside it can be stolen between the probe and the
bind — and then the health probe answers from somebody else's server.

## Scenarios

| name | what it does |
|---|---|
| `idle` | boots, serves nothing, and is watched for the settle window |
| `npm-install` | `pnpm install` of a pinned tree through an npm proxy repo, cold cache then warm |
| `cargo-fetch` | `cargo fetch` of a pinned tree through a Cargo proxy repo, cold then warm |
| `oci-pull` | `docker pull` of a digest-pinned image through an OCI proxy repo, cold then warm |
| `publish` | npm, Cargo, PyPI, Maven, NuGet and OCI publish paths, one server per format |
| `concurrent-reads` | warm npm packument reads at `--concurrency` (50 by default) |
| `s3` | the same publishes and reads on filesystem storage and on MinIO |
| `growth` | 10 000 crate versions published through the API, then the database and storage they cost |

The client passes are pinned: exact dependency versions, a digest-pinned image,
a pinned MinIO release. The publish paths are driven by `curl` over the
documented wire protocol rather than by `npm`, `cargo`, `twine`, `mvn` and
`dotnet`, which keeps a publish figure free of the client's own work and of the
client version's drift; the bytes on the wire are the ones those clients send.
The real clients are exercised elsewhere, by `make test-e2e`.

## Caveats

- **One machine, one run.** Every figure below is a single run on a single
  developer machine. There is no repetition, no confidence interval, and no
  second machine. Treat them as orders of magnitude.
- **The machine's other work is recorded, not controlled.** The load average
  at the start and at the end of a run is in `results.json` and in the report
  header; read it before trusting a latency. The run below was made on an idle
  machine (0.20 rising to 1.48), an earlier one on a machine at load 40 where
  every latency was five to ten times worse while RSS and bytes written barely
  moved.
- **Synthetic artifacts.** Publish and growth scenarios push generated
  packages with incompressible payloads. A real registry holds a different mix
  of sizes, and its compressibility will differ.
- **Client-side cost is not measured.** The sampler watches the opencargo
  process. `pnpm`, `cargo` and `docker` do their own resolution, decompression
  and linking, which is most of the wall time of those scenarios and none of
  the server's CPU.
- **The proxy scenarios need the upstream.** A cold pass measures opencargo
  *and* the round trip to npm, crates.io or Docker Hub, on whatever link the
  machine had that day. Only the warm pass is about opencargo alone.
- **`sccache` and build caches** affect the build, not the run. The binary is
  built by `make release` with the shipped profile (`lto`, `opt-level = "z"`,
  stripped); its size is in the table.
- **Bytes written include the log.** The default `RUST_LOG` is the shipped
  one. `log_bytes` per record says how much of `write_bytes` was logging.

## Results

One run, `scripts/bench.sh --with-image`, at commit `aa69c3e` (the only file
the tree carried uncommitted was this page). AMD Ryzen 5 9600X, 12 threads,
30 GiB RAM, Linux 7.0.0-31, work directory on ext4, rustc 1.93.0, Docker
29.6.2, pnpm 10.25.0. Load average 0.20 at the start and 1.48 at the end: the
machine was otherwise idle. Settings: 15 s settle, concurrency 50, 1 000 reads,
50 publishes per format, 10 000 versions, 16 KiB payloads, the shipped
`RUST_LOG`.

| scenario | storage | wall (s) | peak RSS (MiB) | steady RSS (MiB) | CPU avg % | CPU peak % | CPU (s) | written (MiB) | db (KiB) | storage (MiB) | reqs | err | p50 (ms) | p95 (ms) |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| idle | fs | 0 | 16.2 | 16.2 | 0.0 | 0.0 | 0.000 | 0 | 1991 | 0 | n/a | n/a | n/a | n/a |
| npm-install-cold | fs | 1.2 | 91.3 | 91.3 | 2.0 | 89.3 | 0.330 | 21 | 4881 | 14.7 | 210 | 0 | n/a | n/a |
| npm-install-warm | fs | 1.12 | 142.3 | 142.3 | 1.0 | 69.2 | 0.170 | 4.3 | 4929 | 14.7 | 210 | 0 | n/a | n/a |
| cargo-fetch-cold | fs | 2.89 | 24.9 | 24.9 | 1.4 | 19.9 | 0.260 | 24.4 | 4611 | 19.2 | 118 | 0 | n/a | n/a |
| cargo-fetch-warm | fs | 1.64 | 26.9 | 26.9 | 0.8 | 29.8 | 0.130 | 3.6 | 4631 | 19.2 | 118 | 0 | n/a | n/a |
| oci-pull-cold | fs | 3.47 | 20.8 | 20.8 | 3.4 | 128.9 | 0.640 | 124.8 | 2313 | 124.4 | 16 | 0 | n/a | n/a |
| oci-pull-warm | fs | 1.53 | 22.1 | 22.1 | 1.3 | 109.0 | 0.210 | 0.3 | 2502 | 124.4 | 16 | 0 | n/a | n/a |
| publish-npm | fs | 0.16 | 17.3 | 17.3 | 0.1 | 19.9 | 0.020 | 3.9 | 4612 | 0.5 | 30 | 0 | 4.64 | 9.29 |
| publish-cargo | fs | 0.26 | 17.8 | 17.8 | 0.3 | 19.8 | 0.040 | 5.5 | 4599 | 0.8 | 50 | 0 | 4.82 | 7.56 |
| publish-pypi | fs | 0.19 | 17.9 | 17.9 | 0.2 | 19.9 | 0.030 | 4.1 | 4620 | 0.5 | 30 | 0 | 5.92 | 8.88 |
| publish-maven | fs | 0.85 | 18.2 | 18.2 | 0.8 | 29.8 | 0.130 | 15.7 | 4708 | 0.8 | 150 | 0 | 5.52 | 11.74 |
| publish-nuget | fs | 0.25 | 18.1 | 18.1 | 0.2 | 19.9 | 0.030 | 5.6 | 4656 | 0.8 | 50 | 0 | 4.62 | 7.23 |
| publish-oci | fs | 2.71 | 20.4 | 20.4 | 1.1 | 9.9 | 0.190 | 18.2 | 4692 | 0.8 | 50 | 0 | 53.11 | 56.18 |
| warm-reads-c50 | fs | 2.54 | 86.8 | 86.8 | 13.8 | 129.0 | 2.410 | 12.4 | 4579 | 2.8 | 1000 | 0 | 102.95 | 110.56 |
| fs-publish | fs | 0.26 | 17.6 | 17.6 | 0.1 | 9.9 | 0.010 | 3.9 | 4612 | 0.5 | 30 | 0 | 7.97 | 13.10 |
| fs-read | fs | 2.61 | 20.7 | 20.7 | 1.3 | 19.9 | 0.220 | 7.9 | 4616 | 0.5 | 1000 | 0 | 102.94 | 135.31 |
| s3-publish | s3 | 0.28 | 18.1 | 18.1 | 0.2 | 9.9 | 0.030 | 3.3 | 4612 | 0.9 | 30 | 0 | 8.95 | 12.97 |
| s3-read | s3 | 2.34 | 21.9 | 21.9 | 1.5 | 19.9 | 0.260 | 7.9 | 4616 | 0.9 | 1000 | 0 | 101.96 | 136.44 |
| growth-10000-versions | fs | 72.06 | 40.7 | 40.7 | 24.5 | 59.6 | 21.370 | 630.8 | 12644 | 12.3 | 10000 | 0 | 27.85 | 35.87 |

Throughput, where the harness drove the requests itself:

| scenario | requests/s | downloaded (MiB) |
|---|---|---|
| warm-reads-c50 | 393 | 279.1 |
| fs-read | 383 | 15.9 |
| s3-read | 427 | 15.9 |
| growth-10000-versions | 139 | 0.7 |

Artifacts of this commit: the release binary is 18 122 488 bytes (17.3 MiB,
`lto`, `opt-level = "z"`, stripped); the container image built from the same
tree is 12 044 970 bytes (11.5 MiB, Alpine and a musl build, so smaller than
the glibc binary above).

What the workloads were: the npm tree resolves to 109 packages over 210
requests; the Cargo tree to 59 crates over 118; the pulled image is
`library/postgres@sha256:485935f9…`, 124 MiB of layers over the wire and
160 MiB unpacked locally; the growth scenario is 10 000 versions over 100
crates.

### Reading it

- **An idle server costs 16 MiB and no CPU.** Boot touches 32 MiB before
  settling back; the database of a fresh server with ten repositories is
  2.0 MiB, and the artifact store is empty.
- **Serving is cheap, proxying npm is not.** Every publish path sits between
  4.6 and 5.9 ms at p50 and never moves RSS past 19 MiB. The npm proxy is the
  outlier: peak RSS reaches 91 MiB on the cold install and 142 MiB on the warm
  one, and it stays there for the whole settle window — the steady figure
  equals the peak in every row, so what a burst takes it keeps. Cargo and OCI
  do not do this: a 124 MiB image pull peaks at 21 MiB of RSS, and a Cargo
  fetch of 59 crates at 27 MiB.
- **A warm cache is worth the most where the bytes are biggest.** The warm
  docker pull writes 0.3 MiB instead of 124.8 and takes 1.5 s instead of 3.5;
  warm `cargo fetch` writes 3.6 MiB instead of 24.4. Warm npm install is barely
  faster in wall time (1.12 s against 1.20) because pnpm's own linking, not the
  registry, is what that second is spent on.
- **Fifty concurrent readers are served at 393 requests a second**, 109 MiB/s
  off disk, for 2.4 CPU seconds and a 129% CPU peak — one core and a bit. The
  p50 of 103 ms is a queue of fifty, not a slow request: the same warm reads
  driven one at a time (`--concurrency 1 --reads 100`, a separate run minutes
  later on the same machine) answer at a p50 of 4.2 ms and a p95 of 8.9 ms,
  216 a second.
- **S3 was not slower than the filesystem here**, on MinIO over loopback:
  427 reads a second against 383, and a publish 1 ms dearer. It does cost
  space: the same thirty packages weigh 0.9 MiB in the bucket against 0.5 MiB
  on disk, MinIO keeping its own metadata beside each object.
- **10 000 versions cost 12.6 MiB of database and 12.3 MiB of artifacts** —
  about 1.3 KiB of database per version, for 1 KiB payloads. They took 72 s at
  139 publishes a second, and wrote 631 MiB to do it: fifty times the bytes
  that were kept. The write-ahead log and the index rewrites are the difference,
  along with 2.5 MiB of request logging at the shipped `RUST_LOG`.

### What this run does not say

- Nothing here was repeated. One run, one machine, no interval.
- Publish rows are `curl` over the wire protocol. `npm`, `cargo`, `twine`,
  `mvn` and `dotnet` are not in these numbers; `make test-e2e` runs them, and
  nothing here says what they add.
- The OCI push p50 of 53 ms is a five-request session plus the client process
  starts, and is not comparable to the single-request publish rows.
- npm and PyPI publish rows are thirty deep, not fifty: the server allows
  thirty publishes a minute per user, hard-coded, and a deeper row would have
  measured the limiter.
- `written (MiB)` is what the process sent to the block layer, logging
  included; `log_bytes` in `results.json` says how much was the log.
- No PostgreSQL, no second machine, no cluster, no TLS, no concurrent mixed
  workload, no measurement of what a client spends.
