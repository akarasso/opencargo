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

One run, `make bench`, at commit `ccfa134`. AMD Ryzen 5 9600X, 12 threads,
30 GiB RAM, Linux 7.0.0-31, work directory on ext4, rustc 1.93.0, Docker
29.6.2, pnpm 10.25.0. Load average 2.24 at the start and 2.43 at the end: the
machine was otherwise idle. Settings: 15 s settle, concurrency 50, 1 000 reads,
50 publishes per format, 10 000 versions, 16 KiB payloads, the shipped
`RUST_LOG`.

Two figures moved against the previous run of this page, and in opposite
directions:

| | before | now |
|---|---|---|
| `npm-install-warm`, peak RSS | 142.3 MiB | **44.4 MiB** |
| `growth-10000-versions`, written | 630.8 MiB | **419.2 MiB** |
| `idle`, RSS | 16.2 MiB | **19.4 MiB** |

The first two are the two fixes that shipped in the same release, reproduced
here independently of the branches that made them: the packument path no longer
builds a tree over the whole document, and the publish path checkpoints on a
wider window with two redundant indexes gone.

The third is the cost of what else shipped: routing keeps a rule snapshot in
memory, search keeps an index of what the proxy has served, and tokens carry a
scope. **An idle server is three megabytes heavier than it was**, and that is
the honest half of the same release.

| scenario | storage | wall (s) | peak RSS (MiB) | steady RSS (MiB) | CPU avg % | CPU peak % | CPU (s) | written (MiB) | db (KiB) | storage (MiB) | reqs | err | p50 (ms) | p95 (ms) |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| idle | fs | 0 | 19.4 | 19.4 | 0.0 | 0.0 | 0.000 | 0 | 2732 | 0 | n/a | n/a | n/a | n/a |
| npm-install-cold | fs | 1.93 | 42.9 | 42.9 | 2.5 | 69.3 | 0.420 | 32.5 | 12738 | 20.5 | 210 | 0 | n/a | n/a |
| npm-install-warm | fs | 1.37 | 44.4 | 44.4 | 0.7 | 19.8 | 0.110 | 8.1 | 18100 | 20.5 | 210 | 0 | n/a | n/a |
| cargo-fetch-cold | fs | 2.85 | 29.3 | 29.3 | 1.9 | 29.7 | 0.330 | 25.5 | 7785 | 19.2 | 118 | 0 | n/a | n/a |
| cargo-fetch-warm | fs | 1.69 | 31.7 | 31.7 | 1.0 | 29.6 | 0.160 | 3.4 | 10320 | 19.2 | 118 | 0 | n/a | n/a |
| oci-pull-cold | fs | 3.6 | 23.5 | 23.5 | 3.9 | 108.7 | 0.730 | 124.8 | 3058 | 124.4 | 16 | 0 | n/a | n/a |
| oci-pull-warm | fs | 1.6 | 24.6 | 24.6 | 1.3 | 148.5 | 0.220 | 0.3 | 3255 | 124.4 | 16 | 0 | n/a | n/a |
| publish-npm | fs | 0.17 | 20.9 | 20.8 | 0.3 | 29.7 | 0.040 | 2.6 | 4587 | 0.5 | 30 | 0 | 4.79 | 7.98 |
| publish-cargo | fs | 0.3 | 21.6 | 21.6 | 0.3 | 19.8 | 0.050 | 4 | 5419 | 0.8 | 50 | 0 | 5.30 | 8.00 |
| publish-pypi | fs | 0.3 | 21.1 | 21 | 0.3 | 19.8 | 0.050 | 3 | 4844 | 0.5 | 30 | 0 | 9.96 | 11.96 |
| publish-maven | fs | 1.56 | 21.7 | 21.7 | 1.2 | 29.6 | 0.200 | 12.1 | 12135 | 0.8 | 150 | 0 | 11.85 | 17.06 |
| publish-nuget | fs | 0.42 | 20.9 | 20.9 | 0.5 | 29.7 | 0.070 | 4.1 | 5492 | 0.8 | 50 | 0 | 7.97 | 9.02 |
| publish-oci | fs | 2.95 | 23.9 | 23.9 | 1.5 | 19.8 | 0.260 | 15 | 13289 | 0.8 | 50 | 0 | 58.09 | 61.12 |
| warm-reads-c50 | fs | 4.82 | 29.1 | 29.1 | 4.6 | 39.6 | 0.910 | 24.6 | 16834 | 5.6 | 1000 | 0 | 214.93 | 237.90 |
| fs-publish | fs | 0.25 | 21 | 21 | 0.3 | 29.6 | 0.050 | 2.6 | 4587 | 0.5 | 30 | 0 | 7.98 | 8.68 |
| fs-read | fs | 2.64 | 24.6 | 24.6 | 1.7 | 19.8 | 0.300 | 7.9 | 8618 | 0.5 | 1000 | 0 | 110.95 | 137.87 |
| s3-publish | s3 | 0.28 | 21.4 | 21.4 | 0.2 | 9.9 | 0.030 | 2.1 | 4587 | 0.9 | 30 | 0 | 8.94 | 9.94 |
| s3-read | s3 | 2.74 | 25 | 25 | 2.0 | 29.7 | 0.350 | 7.9 | 8618 | 0.9 | 1000 | 0 | 111.96 | 157.06 |
| growth-10000-versions | fs | 79.14 | 44.5 | 44.5 | 30.3 | 69.2 | 28.530 | 419.2 | 23057 | 12.3 | 10000 | 0 | 30.91 | 40.16 |

Throughput, where the harness drove the requests itself:

| scenario | requests/s | downloaded (MiB) |
|---|---|---|
| warm-reads-c50 | 208 | 279.1 |
| fs-read | 379 | 15.9 |
| s3-read | 365 | 15.9 |
| growth-10000-versions | 126 | 0.7 |

Artifacts: the binary this run measured is the workspace's own glibc release
build, 19.9 MiB. The **published** binary is the musl one CI builds with `lto`
and `opt-level = "z"`, stripped — a different artifact, and the figure to quote
for a download size. The two are never interchangeable, and this page says which
one each number came from.

What the workloads were: the npm tree resolves to 109 packages over 210
requests; the Cargo tree to 59 crates over 118; the pulled image is
`library/postgres@sha256:485935f9…`, 124 MiB of layers over the wire and
160 MiB unpacked locally; the growth scenario is 10 000 versions over 100
crates.

### Reading it

- **An idle server costs 19 MiB and no CPU**, three more than the previous
  release: routing holds a rule snapshot, search an index of what the proxy has
  served, and a token carries a scope. The database of a fresh server with ten
  repositories is 2.7 MiB, and the artifact store is empty.
- **Serving is cheap, and proxying npm no longer is the outlier it was.**
  Every publish path sits between 4.8 and 11.9 ms at p50 and never moves RSS
  past 24 MiB. The npm proxy used to peak at 142 MiB on a warm install and keep
  it for the whole settle window; it now peaks at 44 MiB, within a few MiB of
  the cold pass, because a packument is rendered a version at a time and served
  from its cached rendering instead of being parsed into a tree. A 124 MiB image
  pull peaks at 25 MiB, a Cargo fetch of 59 crates at 32.
- **A warm cache is worth the most where the bytes are biggest.** The warm
  docker pull writes 0.3 MiB instead of 124.8 and takes 1.5 s instead of 3.5;
  warm `cargo fetch` writes 3.6 MiB instead of 24.4. Warm npm install is barely
  faster in wall time (1.12 s against 1.20) because pnpm's own linking, not the
  registry, is what that second is spent on.
- **Fifty concurrent readers are served at 208 requests a second**, 58 MiB/s
  off disk, for 0.9 CPU seconds. The p50 of 215 ms is a queue of fifty, not a
  slow request. This run is slower than the previous one (393 a second) and the
  reason is not in the code: the machine carried a load average of 2.2 rather
  than 0.2, and this scenario is the only one in the table that competes for
  CPU. Read it as a floor, not as a regression.
- **S3 was not slower than the filesystem here**, on MinIO over loopback:
  365 reads a second against 379, and a publish 1 ms dearer. It does cost
  space: the same thirty packages weigh 0.9 MiB in the bucket against 0.5 MiB
  on disk, MinIO keeping its own metadata beside each object.
- **10 000 versions cost 22.5 MiB of database and 12.3 MiB of artifacts.**
  They took 79 s at 126 publishes a second, and wrote 419 MiB to do it: thirty-
  four times the bytes that were kept, against fifty-one in the previous
  release. The write-ahead log and the index rewrites are still the difference;
  what changed is a 16 MiB checkpoint window instead of 4, truncated afterwards,
  and two indexes a unique constraint already carried, dropped. See
  [write-amplification.md](write-amplification.md).

### What this run does not say

- Nothing here was repeated. One run, one machine, no interval.
- Publish rows are `curl` over the wire protocol. `npm`, `cargo`, `twine`,
  `mvn` and `dotnet` are not in these numbers; `make test-e2e` runs them, and
  nothing here says what they add.
- The OCI push p50 of 53 ms is a five-request session plus the client process
  starts, and is not comparable to the single-request publish rows.
- npm and PyPI publish rows are thirty deep, not fifty: the shipped
  `[limits.publish]` allows thirty publishes a minute per user, and a deeper row would have
  measured the limiter.
- `written (MiB)` is what the process sent to the block layer, logging
  included; `log_bytes` in `results.json` says how much was the log.
- No PostgreSQL, no second machine, no cluster, no TLS, no concurrent mixed
  workload, no measurement of what a client spends.
