# Changelog

All notable changes to this project will be documented in this file.

## [Unreleased]

### Added
- S3-compatible artifact storage (`[storage] backend = "s3"`), a preview:
  validated against MinIO, not yet against a hosted provider. Credentials
  come only from an allowlisted environment; TLS trusts the compiled-in
  Mozilla roots. See `docs/storage.md`.
- `opencargo storage check|verify|migrate|reclaim`, and
  `GET /api/v1/system/storage` with a Storage tile on the System page.
- OCI: `GET` on an upload location reports its range; the upload `POST`
  answers `OCI-Chunk-Min-Length`.
- Maven repositories under `/maven/{repo}/`: hosted (deploy with `mvn` or
  Gradle's `maven-publish`), proxy and group. A deposit is visible once its
  POM lands; checksums served are always computed by the server, and a
  declared checksum that disagrees with the file is refused. A version
  deposited without a POM is published after ten minutes unless another
  user contested it, in which case an administrator decides through
  `POST /api/v1/maven/{repo}/decide`.
- The `maven` repository format. Migration 024 admits it and refuses to run
  while a repository is named `maven`, because `/maven/` becomes the Maven
  endpoint: rename that repository with the previous release before
  upgrading. `maven` is a reserved repository name from now on.
- Single sign-on through OpenID Connect (Google, Entra ID, GitLab, any
  provider): accounts created or linked on first login, groups mapped to
  roles and grants at every login, credentials revoked with the provider or
  the account, and `password_mode` to retire local passwords. See
  `docs/sso.md`.
- Registry token signing keys survive a restart (`server_secrets`).
- An admin can disable and re-enable an account; its SSO credentials are
  revoked at once and every credential it holds is refused.

### Changed
- OCI: a `PATCH` whose `Content-Range` does not start where the upload
  stands is `416` with the current `Range`; an unknown upload id, or one
  started in another repository, is `404 BLOB_UPLOAD_UNKNOWN`; more than
  10 000 chunks in one upload is `413`; upload, digest and manifest refusals
  carry an `errors[]` body.
- OCI: a manifest listing a config or layer blob the repository does not
  hold is refused with `400 MANIFEST_BLOB_UNKNOWN` (it used to be stored).
- OCI uploads started before this version are dropped by the storage sweep;
  a client restarts them.
- Deleting a manifest, a blob or a version, evicting a proxy entry or
  removing a repository no longer frees its bytes at once: the keys are
  queued and reclaimed by the storage sweep after a two-hour grace.
- New artifacts are stored under an opaque per-repository incarnation and a
  digest-keyed path (`r/<incarnation>/…`); existing files keep their paths
  and keep serving.
- Deprecated: legacy `.part-` scratch files beside finished objects are
  still skipped by listings and swept; that carve-out will be removed in a
  later release.
- A credential that does not verify is refused with 401 on every route,
  including an invalid or revoked Bearer on a public repository, which used
  to fall back to anonymous. A request with no credential is still anonymous.
- An API token is accepted as the Basic password on every format and is
  never throttled by its account's password lockout. Password failures are
  counted per account across Basic, `npm login` and the password change;
  token failures are counted per client address. `auth.trusted_proxies`
  names the proxies whose `X-Forwarded-For` is believed.

## [0.1.0-rc.1] - 2026-09-17

First tagged release candidate.

### Added
- Signed, attested releases: a tag `vX.Y.Z` or `vX.Y.Z-rc.N` publishes static
  `x86_64` and `aarch64` musl binaries, a CycloneDX SBOM per binary and
  `SHA256SUMS`, each with a Sigstore keyless bundle, plus GitHub build
  provenance and SBOM attestations. The container image `X.Y.Z[-rc.N]` is the
  image main CI built and scanned for that commit, re-scanned by Trivy,
  signed and copied by digest, never rebuilt. See "Verifying a release" in
  the README.
- `proxy` and `group` repositories for every format. Cargo: sparse index
  proxied and revalidated by ETag, crates fetched through the upstream `dl`
  template and verified against the index checksum, group index as the
  union of its members (`Warning: 199` when one is down), tokenless
  `config.json` with `auth-required`. Go: GOPROXY proxy with escaped module
  paths, group `@v/list` union and `@latest` by semver, upstream 410 as 404.
  OCI: pull-through proxy with the Docker Hub Bearer token dance, `library/`
  prefix, streamed blobs verified by digest, HEAD served from the cache,
  group manifests, blobs and merged tag lists. npm: dist-tags and search
  through proxies and nested groups. Every proxy caches immutable artifacts
  forever (npm tarballs and crates up to 100 MiB, Go zips 512 MiB, OCI blobs
  4 GiB), npm packuments, cargo `config.json` and OCI tags for
  `proxy.default_ttl`, cargo index lines, Go queries and OCI tag lists for
  ten minutes, upstream 404s for `proxy.negative_cache_ttl`, and serves a
  stale copy with `Warning: 110` when the upstream is down. Concurrent
  misses are deduplicated in-process; every body is streamed to disk.
- Nested OCI image names (`team/app`, `org/team/app`) on every `/v2` route,
  including `Location` and `Docker-Content-Digest` headers.
- OCI Bearer token auth: `GET /v2/` without credentials and every `401` on
  a registry route under `/v2/` carry `WWW-Authenticate: Bearer
  realm=".../v2/token",service="opencargo"` (plus the image's `scope`; the
  token endpoint's own `401` carries none), and `GET /v2/token`
  exchanges Basic credentials for a one-hour signed token, or issues an
  anonymous one when `anonymous_read` is on. Docker daemons up to 28.x only
  send credentials after such a challenge, so `docker push` after
  `docker login` failed with `unauthorized` on those clients; answering
  `Basic` on the ping would have broken their anonymous pulls instead. Basic
  auth and API tokens are still accepted on every `/v2/` route.
- Per-repository upstream credentials (`upstream_auth`, or
  `OPENCARGO_UPSTREAM_AUTH_<REPO>` = `basic:user:pass` / `bearer:token`,
  read at startup for every repository, API-created ones included), sent
  only to the upstream host and to `token_realms`; upstream bearer tokens
  are cached per repository. `dl_allow_private` (or
  `OPENCARGO_DL_ALLOW_PRIVATE_<REPO>=1`) allows a Cargo `dl` or a token realm
  whose host is, or resolves to, a private address, which a proxy over a
  local opencargo needs; otherwise such hosts, and redirect hops off the
  upstream's origin, are refused.
- Per-advisory vulnerability severity from the full OSV record
  (`database_specific.severity`, else the highest CVSS 3.x/4.0 vector, `MAL-`
  ids critical); `vuln_scan.block_on_critical` now refuses the publish with
  `400` before anything is written, `vuln_scan.fail_closed` turns an OSV
  outage into a `503`, `osv_base_url` / `--osv-base-url` / `max_concurrency`
  are configurable, and advisories are fetched once per process. The UI
  shows a severity chip per advisory.
- `[cleanup] proxy_cache_older_than_days` (default 30) evicts idle proxy
  cache entries and abandoned partial downloads, even with `enabled = false`.
- Repository validation on create, update and config seed: names match
  `[a-z0-9][a-z0-9._-]{0,63}` without `..`, a proxy needs an `http(s)`
  upstream, a group needs non-empty members of its own format, nesting is
  capped at 5 and cycles are refused. Changing a proxy's upstream purges its
  cache. Deleting a member of a group is `409`; `purge-cache` on a group
  purges its proxy members, deleting a group leaves them alone.
- `tags/list` answers a partial page with `Link: <...>; rel="next"`.
- Prometheus counters `opencargo_downloads_total`, `opencargo_publishes_total`,
  `opencargo_cache_hits_total` and `opencargo_cache_misses_total` are emitted.
- Real-client end-to-end tests for cargo, go and the docker CLI through a
  group fronting a second instance; CI runs them with `OPENCARGO_E2E_REQUIRE=1`.
- `LICENSE` (MIT) and `SECURITY.md` (private vulnerability reporting, scope,
  operator hardening checklist)
- English `README.md` (benefits first, one-command install, honest comparison
  with Forgejo Packages / Nexus / Verdaccio / Harbor / Artifactory); the
  previous French README moved to `README.fr.md`; API reference in `docs/api.md`
- Real-time event WebSocket at `/api/v1/events/ws` (first-frame token auth,
  server-side visibility scoping public/authenticated/admin, heartbeat,
  periodic token re-validation, `resync` marker on lag)
- `GET /api/v1/me/permissions`: effective per-repository rights for the
  caller, with the rule that produced them (`admin`/`grant`/`role`/`anonymous`)
- `whoami` now returns `role` and `must_change_password`
- Audit entries for previously silent mutations: user update, repository
  create/update/delete/purge-cache, webhook create/update/delete,
  permission set/remove
- Web UI: per-user × per-repository permission matrix editor, "My access"
  page, repository CRUD, webhook CRUD + test delivery, live audit stream,
  command palette (Cmd+K), live dashboard manifest fed by the WebSocket

### Changed
- Images pushed by main CI (`sha-<commit>`, `latest`) are signed keyless by
  digest and carry a build provenance attestation; `latest` only moves when
  the commit is still main's head. Every GitHub Action is pinned by commit SHA.
- Proxy cache storage moved to a `proxy_cache_entries` table (migration 013)
  with streamed writes; the legacy npm cache layout and `proxy_cache_meta`
  rows are removed on the next purge or delete of the repository.
- Repository types and formats are typed end to end; `package.published`
  events carry the format name (`cargo`, `go`) rather than the OSV ecosystem.
- An OCI upstream's `401`/`403` after a token is a `404` that is asked again
  on the next request, never a negative cache row.
- npm reads accept legacy uppercase names (`JSONStream`); publish keeps the
  lowercase rule. Cargo index lines carry the sparse-index dependency shape
  (`req`, `package`) and hosted crates resolve regardless of case. Go reads
  accept case-escaped versions (`v1.0.0-!r!c1`).
- Hosted files are written through a part file and renamed, so a re-push
  never truncates a reader; `Content-Length` always describes the file
  being streamed.
- The `Go` `.info` `Time` is RFC 3339, `@latest` picks the highest semver and
  an unknown module's `@v/list` is `404` instead of an empty `200`.
- A `proxy` or `group` repository whose upstream is unreachable or answers
  anything other than 404/410 now returns 502 Bad Gateway instead of 404, so
  npm/pnpm report a fetch error rather than E404. An upstream 404/410 is
  still a 404 (negative-cached), and a stale cached copy is served with
  `Warning: 110` before failing.
- Web UI redesigned end to end (new design system, self-hosted IBM Plex /
  Space Grotesk, inline SVG icons, skeleton loaders, mobile drawer); frontend
  split into a framework-agnostic `core/` layer (typed API client, WebSocket
  client, reactive stores) and rendering components
- `GET /api/v1/repositories` returns `type`, `format`, `visibility` and
  `upstream`, and no longer lists private repositories to anonymous callers
- Dashboard stats apply the same visibility rules as the package list
  (anonymous callers no longer see private version/download/repo counts)

### Security
- A public `group` no longer serves metadata, tarballs or search results of a
  private member to callers without read access on that member. Members are
  filtered by the caller's rights; unreadable ones are skipped.
- `ammonia`, `h2` and `rustls` bumped past their RustSec advisories; CI pins
  Rust 1.93.0 and Trivy blocks the image push on fixable HIGH/CRITICAL findings.

### Fixed
- Unknown routes under `/v2/` and `/api/` answer a JSON `404` instead of the
  web UI's HTML; Docker 29's referrers probe made every pull through opencargo
  fail with "failed to decode referrers index".
- Unscoped npm packages (`lodash`) had no metadata route and fell through to
  the web UI, so installing a public package through a proxy never worked with
  a real client.
- Tarball URLs served through a `group` pointed at the member repository;
  clients with a token declared on the group path only (pnpm) got 401 once
  anonymous reads were disabled. They now point at the repository the client
  asked for.
- `--base-url` / `OPENCARGO_BASE_URL` override the public URL; a warning is
  logged when listening on a non-loopback address with a `localhost` base URL.
- The phantom `pypi` format is gone and a config-seeded repository that fails
  validation stops startup instead of silently serving nothing.
- A blob `HEAD` forwarded to an upstream answered `Content-Length: 0`.
- reqwest moved to rustls with a single crypto provider; OpenSSL is out of the
  container build and the image binary shrank from 17 MB to 11 MB.
- Container image: `/data` is pre-created and owned by the runtime user, the
  default command listens on `0.0.0.0`, so `docker run -v x:/data ghcr.io/akarasso/opencargo`
  works without a config file (it previously exited with "Permission denied")
- Helm chart default image repository pointed at a non-existent registry path
- Production CSP silently blocked Google Fonts and Material Symbols; fonts
  are now bundled and icons inlined, so typography and iconography render
  under the strict CSP

## [0.1.0] - 2026-03-23 (initial version, never tagged; the first tagged release will be v0.1.0-rc.1)

### Added
- npm package registry (publish, install, search, dist-tags)
- Cargo crate registry (sparse protocol, publish, download, yank/unyank)
- OCI/Docker container registry (blobs, manifests, tags)
- Go module registry (GOPROXY protocol)
- Proxy repositories with transparent caching (npmjs.org, etc.)
- Group repositories (merge multiple repos behind one URL)
- Package promotion between hosted repos (dev -> prod workflow)
- Full authentication system (users, API tokens, roles)
- Secure initial admin password (random generation, file or env var)
- Rate limiting on sensitive endpoints
- Dependency graph tracking and impact analysis
- Webhooks for package events (publish, promote)
- Vulnerability scanning via OSV.dev
- Web UI (SolidJS SPA with Stitch-designed dark theme)
- Prometheus metrics endpoint
- Full-text search (SQLite FTS5)
- Automatic cleanup policies
- TLS native support (rustls)
- Health check endpoints (liveness + readiness)
- Helm chart and Kubernetes manifests
- CI sidecar mode for build caching
- Audit logging
- Docker multi-stage build
- 60+ integration tests including pnpm E2E
