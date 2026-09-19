# Write amplification

What a publish writes beyond what it keeps, and what was done about it.

Publishing 10 000 crate versions writes 12.3 MiB of artifacts and about 6 MiB
of database, and used to send 670 MiB to the disk doing it — fifty-four times
what it kept. This page is where those bytes were, which of them were
avoidable, and the figure after removing them. The same run on an idle
machine had measured 631 MiB, fifty-one times what it kept; the shape is the
machine's, the ratio is not.

## How it was measured

`scripts/bench.sh --scenarios growth --versions 10000` (the harness of the
`feat/bench` branch), which boots a release binary on a scratch directory,
publishes 10 000 versions over 100 crates at concurrency 8 with 1 KiB
payloads, and reads `write_bytes` from `/proc/<pid>/io` after `sync` — what
the process sent to the block layer, artifacts, database, write-ahead log and
its own log together.

Attribution below the process is the write-ahead log itself: every page a
transaction dirties is appended to it whole, with the page number in the frame
header, so counting frames by page and mapping the page to its table through
`dbstat` says which b-tree each byte belonged to. Both runs are one run on one
machine (AMD Ryzen 5 9600X, ext4), taken minutes apart under the same load
band; the machine was busy, which moves wall time and latency and leaves bytes
written alone.

## Where the bytes were

One cargo publish is two write transactions, twelve dirtied pages, 48 KiB of
write-ahead log:

| b-tree | pages per publish | why |
|---|---|---|
| `reclaim_pins` table, its `token` primary key, `physical_key` and `until` indexes | 8 | four b-trees, written when the placement's pin is taken and again when the commit spends it |
| `versions`, `UNIQUE(package_id, version)` | 2 | the row itself |
| `idx_versions_package` | 1 | `versions(package_id)`, the leading column of the unique index beside it |
| `sqlite_sequence` | 1 | `AUTOINCREMENT` on `versions` |

The remaining 20 KiB per publish is the artifact's own block, the checkpoint
copying dirty pages into the database (94 distinct pages per 1000-page window,
about 6 KiB per publish), the 258-byte log line, and the tail block each
`fsync` rewrites at a commit boundary.

## What changed

- **Migration 027 — the pin table is its own expiry index.** A placement's pin
  is a row inserted and, seconds later, deleted; it was carrying four b-trees.
  Rebuilt `WITHOUT ROWID` under `PRIMARY KEY (until, token)`, the table is the
  index the sweep reads as a range, and only `physical_key` keeps an index of
  its own. Two b-trees, four pages instead of eight. Spending a pin names the
  physical key first so the delete rides that index; the token stays in the
  predicate, because the token is what fences.
- **Migration 028 — two indexes a unique constraint already carried.**
  `idx_versions_package` beside `UNIQUE(package_id, version)` and
  `idx_dist_tags_package` beside `UNIQUE(package_id, tag)`: a lookup or an
  order by package uses the unique index either way.
- **A wider checkpoint window.** A checkpoint copies each page the log holds
  once however many times the log holds it, so the window decides how often
  the same hot leaf reaches the database: 4000 pages instead of 1000, a
  quarter of the copies. `journal_size_limit` now bounds what a burst leaves
  behind, which the default never did.
- **A log nobody colours.** The subscriber was built with ANSI on whatever it
  wrote to: 38% of every line of a server's log was escape sequences. Colour
  follows `stdout().is_terminal()`, and the default filter drops `tower_http`,
  which has logged nothing since the per-request `TraceLayer` was replaced by
  the middleware that records a metric.

## The result

10 000 versions, same harness, same machine, same settings:

| | before (`25ffc25`) | after |
|---|---|---|
| written | 670.6 MiB | 422.6 MiB |
| per publish | 68.7 KiB | 43.3 KiB |
| pages dirtied per publish | 12.4 | 7.4 |
| server log | 2.46 MiB | 1.53 MiB |
| database file | 6.51 MiB | 6.40 MiB |
| write-ahead log, resident | 10.7 MiB | 17.3 MiB |
| artifacts kept | 12.3 MiB | 12.3 MiB |
| amplification | 54x | 34x |

A 37% cut, and the one figure that moves the other way is deliberate: the
wider window leaves more of the log resident between checkpoints, capped at
the 64 MiB `journal_size_limit` rather than the previous nothing.

## What was left alone, and why

- **`synchronous` stays `FULL`.** Every commit is fsynced into the log before
  its client is answered. Dropping to `NORMAL` would have saved fsyncs, not
  bytes, and it is exactly the durability this page is not allowed to spend.
- **The pin stays two transactions.** It must be durable before the bytes are
  written and spent by the transaction that claims them: that ordering is the
  fence, not an accident of the schema.
- **`AUTOINCREMENT` on `versions` stays**, at one `sqlite_sequence` page per
  publish. It is what keeps a version id from being reused, and a reused id
  reattaches whatever still names the old one.
- **The 4 KiB page stays.** A smaller page would divide the bytes a dirty page
  costs, and it needs a `VACUUM` of the whole database to change, on every
  installation, to trade write cost for read cost on rows that are mostly
  bigger than a page fragment.
- **One log line per publish stays.** It is the registry's own record of a
  write; at 161 bytes it is 0.4% of what a publish writes.

## The guard

`tests/write_amplification_test.rs` counts the frames twenty publishes append
to the write-ahead log and fails above eight pages each, and runs the shipped
binary to assert its log carries no escape sequence at the default level.
Neither reads a clock. The ceiling is the measured cost plus the margin one
page split needs: it comes down when the cost does.
