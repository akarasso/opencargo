# Migrating from another registry

`opencargo import` copies an existing registry into opencargo **hosted**
repositories and reports, honestly, what it could not copy. It is an HTTP
client on both ends: it reads the source over its API and publishes into the
target over opencargo's own protocols, so every import goes through the same
name rules, checksums, permission checks, vulnerability gate, events and
webhooks as any publish. It runs from anywhere that reaches both registries.

Status: preview. Tested against in-process fakes of every source, a second
opencargo, and real Verdaccio 6, Nexus OSS 3.76 and `registry:2` containers
(`make test-e2e-import`); not yet validated against Artifactory, GitHub
Packages or a production-sized source.

## Sources

| `--source` | Listing | Formats copied |
|---|---|---|
| `verdaccio` | `/-/v1/search` paged to a short page, a packument per package | npm |
| `nexus` | components API, one stream per repository, `continuationToken` | npm, cargo, go, docker |
| `artifactory` | AQL per local repository, offset paging | npm, cargo, go, docker |
| `github` | REST packages per owner (`--source-repo ORG`) and type | npm, container |
| `distribution` | `tags/list` of each `--source-repo` image; no catalogue | OCI |

npm, whoever serves it, is read off the source's own npm endpoint, so
dependencies, scripts, licence, description and README survive. cargo is
read off the source's sparse index (dependencies, features, `links`,
`rust_version`, checksum, yank state) with description and licence from the
`.crate`'s manifest; a source without an index falls back to the manifest
and reports that yank state is unknown. go modules are published under their
raw path. OCI images are copied blob by blob, children of an index first; a
moved tag is re-pointed and noted (`--no-retag` refuses instead).

Maven, PyPI and NuGet repositories are listed and reported as
`UnsupportedFormat`: the target serves those formats, the importer has no
sink for them yet. Harbor and Docker Hub sources are not implemented.

## Running it

```bash
export OPENCARGO_IMPORT_SOURCE_USER=admin OPENCARGO_IMPORT_SOURCE_PASSWORD=...   # or _SOURCE_TOKEN
export OPENCARGO_IMPORT_TARGET_TOKEN=...          # a user with read and write on the targets
opencargo import run --source nexus --from https://nexus.internal/ --to https://registry.example.com/ \
  --map 'npm-*=npm' --map 'crates=cargo' --dry-run
opencargo import run ...                          # the same without --dry-run
opencargo import resume --state FILE              # after a failure, a fix or an interruption
opencargo import report --state FILE              # offline, any time later
opencargo import permissions --state FILE [--apply]
opencargo import forget --state FILE
```

Credentials never go on the command line: a `--from` or `--to` carrying
`user:password@` is refused. The source credential is sent only to `--from`
and the endpoints the source adapter itself names (GitHub's npm and container
registries); a URL the source hands over that points elsewhere is fetched
anonymously, and a private address is refused unless `--allow-source-host`
names it. The state file (`.opencargo-import/{source}-{target}.db`, mode
0600) holds coordinates only, never a credential or a signed URL.

Before any byte moves, the run asserts on the target's own permission view
(`GET /api/v1/me/permissions`) that every target repository exists, is
hosted, has the planned format, and that the token can write it and read it.
`--create-repos` creates missing ones with `OPENCARGO_IMPORT_TARGET_ADMIN_TOKEN`
and grants the importing user. npm publishes are paced under the target's
30-per-minute limit (`--target-rate`, 25); a target 429 or 503 parks every
worker instead of failing items.

Every imported version is scanned by the target like any publish: turn
`vuln_scan.block_on_critical` off for the import window if blocking is on,
and wire webhooks afterwards, since each version emits `package.published`.

## The gap report

`report.json` and `report.md` land next to the state file after every run,
aborted ones included. Every row has a kind and each kind one exit class:

| Exit | Kinds |
|---|---|
| 2 | `Failed`, `TargetRefused`, `TooLarge`, `CopiedUnverified`, `UnpublishableName`, `TargetCollision` |
| 4 (0 with `--allow-incomplete`) | `UnsupportedFormat`, `NoTarget`, `ListingIncomplete` |
| 0 | `SkippedUnverifiable`, `SourceOnlyFeature`, `PermissionNotMapped` |

Exit 1 means the run never started (arguments, credentials, preflight),
exit 3 that it stopped early with its state saved (`--fail-fast`, an
interrupt, a target that stayed unavailable). The report describes the
target as it is now: fix a cause, `resume`, and its row disappears.

## Limits

- npm publish bodies are capped at 100 MiB of base64 by the target: about
  74 MiB of tarball (`TooLarge`, nothing sent). go zips are capped at 100 MiB,
  crates at 1 GiB.
- The target holds each npm body in memory while it lands, and a cargo or go
  one too: `--concurrency` (4) bounds how many at once, `--oci-concurrency`
  (1) how many images. No memory budget is computed for you.
- An interrupted OCI upload leaves the bytes already sent under
  `oci/_uploads/` on the target until its own sweep; the item's note says how
  many.
- Verdaccio 6.8 and later clamp the search at offset 10 000, and the GitHub
  API stops at 10 000 entries: both end as `ListingIncomplete`, never as a
  silent partial import.
- `--max-versions` and `--latest-only` apply per listing page on Nexus and
  Artifactory, where one package's versions can span pages.
