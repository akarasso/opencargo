# opencargo

**A self-hosted package registry for npm, Cargo, Docker/OCI and Go modules, in one 10 MB binary.**

Host your private packages, proxy and cache npmjs.org, crates.io, the Go
module proxy and Docker Hub, promote releases from dev to prod, one binary for
the whole team.
No JVM, no Postgres, no telemetry. SQLite inside, about 20 MB of RAM at rest.

[![CI](https://github.com/akarasso/opencargo/actions/workflows/ci.yml/badge.svg)](https://github.com/akarasso/opencargo/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
[![Container image](https://img.shields.io/badge/ghcr.io-akarasso%2Fopencargo-blue)](https://github.com/akarasso/opencargo/pkgs/container/opencargo)

*Version française : [README.fr.md](README.fr.md).*

---

## Try it in one command

```bash
docker run -d --name opencargo -p 6789:6789 \
  -v opencargo-data:/data \
  -e OPENCARGO_ADMIN_PASSWORD=change-me \
  ghcr.io/akarasso/opencargo:latest
```

Open <http://localhost:6789>, log in as `admin`, and create repositories from
the UI. Or script it: the same three calls give you a private npm registry
that falls back to npmjs.org and caches what it fetches.

```bash
R=http://localhost:6789

curl -u admin:change-me -X POST $R/api/v1/repositories -H 'Content-Type: application/json' \
  -d '{"name":"npm-private","type":"hosted","format":"npm","visibility":"private"}'
curl -u admin:change-me -X POST $R/api/v1/repositories -H 'Content-Type: application/json' \
  -d '{"name":"npm-proxy","type":"proxy","format":"npm","visibility":"public","upstream":"https://registry.npmjs.org"}'
curl -u admin:change-me -X POST $R/api/v1/repositories -H 'Content-Type: application/json' \
  -d '{"name":"npm-all","type":"group","format":"npm","visibility":"public","members":["npm-private","npm-proxy"]}'

# A token for your laptop or CI (shown once)
curl -u admin:change-me -X POST $R/api/v1/users/admin/tokens -H 'Content-Type: application/json' \
  -d '{"name":"laptop","expires_in_days":365}'
```

Publish to the hosted repo, install through the group:

```ini
# .npmrc
registry=http://localhost:6789/npm-all/
//localhost:6789/npm-all/:_authToken=trg_...
//localhost:6789/npm-private/:_authToken=trg_...
```

```bash
npm publish --registry http://localhost:6789/npm-private/
npm install @acme/hello lodash        # @acme from you, lodash proxied and cached
```

Running it on a server rather than your laptop? Add
`-e OPENCARGO_BASE_URL=https://registry.example.com`: package metadata embeds
absolute download URLs, and they default to `http://localhost:6789`.

Without `OPENCARGO_ADMIN_PASSWORD`, a random admin password is generated at
first boot. Read it with `docker exec opencargo cat /data/admin.password`; the
UI will make you change it at first login.

---

## Why another registry

Teams of 5 to 60 developers usually end up with one of these:

- **Artifactory or Nexus**: does everything, needs a JVM, gigabytes of RAM and
  someone to babysit it. Priced for enterprises.
- **GitHub or GitLab Packages**: tied to a repo, no real upstream proxy, no
  promotion, permissions follow the repo rather than the package.
- **Verdaccio + Harbor + a Go proxy**: three services, three auth systems,
  three things to upgrade.

opencargo is the fourth option: one static binary that serves the four
formats a polyglot team actually uses, with the features the "light" options
lack (proxy, group, promotion, per-user permissions) and without the weight of
the enterprise ones.

## What it does today

- **Formats**: npm (incl. scoped packages, dist-tags, `npm login`), Cargo
  (sparse index, yank/unyank), OCI Distribution v2 (Docker push/pull),
  Go modules (GOPROXY).
- **Repository types**: `hosted` (you publish), `proxy` (transparent cache of
  an upstream) and `group` (one URL in front of several repos, ordered
  resolution), for all four formats. Metadata is cached for a TTL and
  revalidated with ETags, immutable artifacts (tarballs, crates, module zips,
  blobs) forever; a stale copy is served with `Warning: 110` when the
  upstream is down, and an unreachable upstream is a `502`, never a silent
  `404`.
- **Promotion**: move a version from `dev` to `prod` without re-uploading or
  changing lockfiles; full audit trail.
- **Permissions**: roles plus a per-user × per-repository matrix, editable in
  the UI, enforced server-side and on the event stream.
- **Dependency graph**: dependencies extracted at publish time; "who depends on
  this?" and impact analysis before you delete a version.
- **Vulnerability scanning** through [OSV.dev](https://osv.dev) on every
  publish once enabled (`[vuln_scan] enabled = true`, off by default), with
  a per-advisory severity (OSV label, else CVSS 3.x/4.0 score) and an
  optional block of critical publishes before anything is written.
- **Webhooks** with HMAC signatures, **WebSocket event stream**, **Prometheus
  metrics**, full-text search, rate limiting, native TLS.
- **Web UI** embedded in the binary: live dashboard, package pages with
  README and install snippets, admin screens, Cmd+K palette.
- **Ops**: Dockerfile, Kustomize manifests, Helm chart, CI sidecar mode for
  caching in GitHub Actions / GitLab CI runners.

Argon2 for passwords, hashed API tokens, path-traversal guards and the
permission matrix are covered by integration tests. See [SECURITY.md](SECURITY.md).

## Where it is going

The registry is the foundation. The next layer is a **dependency policy
engine, in audit mode first**: rules versioned with your code
(`package.age < 72h`, `cve.severity >= high`, `install_scripts && !allowlisted`,
`license in [AGPL]`), evaluated at resolution time, scoped per repository so
internal packages are not judged like public ones. The first deliverable is a
weekly report of *what would have been blocked*, before anything is actually
blocked. Then migration importers from Nexus / Artifactory / Verdaccio /
GitHub Packages, and governance of MCP servers and agent skills distributed
through npm, PyPI and OCI.

The registry, audit mode and OIDC SSO are and will stay MIT. Organisation-level
enforcement (quarantine, approvals, audit exports, compliance reports)
is planned as a paid add-on for self-hosted deployments. If you run opencargo
and would talk for 30 minutes about what you would want blocked, open an
issue or write to the address in `SECURITY.md`.

## Comparison

| | opencargo | Forgejo / Gitea Packages | Nexus Repository CE | Verdaccio | Harbor | JFrog Artifactory |
|---|---|---|---|---|---|---|
| Formats | npm, Cargo, OCI, Go | 20+ | 15+ | npm only | OCI, Helm | 30+ |
| Upstream proxy + cache | npm, Cargo, Go, OCI | no | yes | yes | yes | yes |
| Group / virtual repos | npm, Cargo, Go, OCI | no | yes | n/a | no | yes |
| Promotion dev → prod | yes | no | paid | no | replication | yes |
| Per-user × per-repo permissions | yes | per forge repo | yes | basic | project-level | yes |
| Vulnerability scan | OSV, built in | no | paid (Firewall) | no | Trivy | paid (Xray) |
| Footprint | 1 binary, SQLite, ~20 MB RAM | part of a forge | JVM, 2 GB+ RAM | Node.js | 8+ containers, Postgres, Redis | JVM, 4 GB+ RAM |
| License | MIT | MIT | EPL, usage caps | MIT | Apache-2.0 | proprietary |

## Known limitations

Read this before the comparison table sells you anything.

- PyPI, Maven and NuGet (hosted, proxy, group), S3-compatible storage and
  OIDC SSO are new and in preview: tested in CI, not yet validated on a
  second deployment. The comparison table above does not count them yet.
- The Go checksum database is not proxied: exclude private modules with
  `GONOSUMDB` or run with `GOSUMDB=off`. `go` gets a `404` for an unknown
  module and moves on to the next `GOPROXY` entry, but a `502` (upstream down)
  stops it rather than falling back to `direct`.
- Upstream credentials (`upstream_auth`, `OPENCARGO_UPSTREAM_AUTH_<REPO>`) are
  sent to the upstream host and to the `token_realms` you list, nothing else;
  a Cargo `dl` or a Bearer `realm` whose host is, or resolves to, a private
  address is refused unless the repository opts in (`dl_allow_private`).
  Names are resolved once before the request: DNS rebinding is not mitigated.
- A proxied npm tarball or crate larger than 100 MiB is refused with `502`
  (Go zips are capped at 512 MiB, OCI blobs at 4 GiB); nothing stale is
  served in that case.
- An OCI upstream that answers `401`/`403` after issuing a token is treated
  as "image unknown": the client gets a `404`, so a wrong pull credential
  looks like a missing image. The refusal is not cached, so it clears as soon
  as the credential does.
- A cold blob is written to disk in full before the first byte reaches the
  client (bounded by the read timeout, not by size). Pushes are capped at
  1 GiB per request, so a proxied 3 GiB layer pulls but cannot be re-pushed.
- Deduplication of concurrent downloads, upstream tokens and OSV advisories
  is per process: several replicas behind one load balancer each fetch their
  own copy.
- Vulnerability severity is read per advisory from the full OSV record: a
  `database_specific.severity` label wins, else the highest CVSS 3.x/4.0
  vector is scored, and `MAL-` ids are critical. `vuln_scan.block_on_critical`
  refuses such a publish before anything is written; `vuln_scan.fail_closed`
  turns an OSV outage into a 503 instead of an unscanned publish. Advisories
  with only CVSS 2 data (or none) are reported as `unknown` and never block.
- One maintainer, pre-1.0. Pin the image by digest and keep backups of `/data`.

---

## Client configuration

### npm / pnpm / yarn

```ini
# .npmrc
@acme:registry=http://registry.example.com/npm-all/
//registry.example.com/npm-all/:_authToken=trg_...
```

`npm login --registry http://registry.example.com/npm-all/` also works.

A `group` such as `npm-all` answers `npm install` and `npm dist-tag ls` from
its hosted members first, then from its proxies (dist-tags come from the
cached packument); `npm search` covers hosted members only, nested groups
included, so proxied packages are not searchable. `npm publish` and
`npm dist-tag add|rm` are accepted on hosted repositories only.

### Cargo

```toml
# .cargo/config.toml
[registries.private]
index = "sparse+http://registry.example.com/cargo-private/index/"
token = "Bearer trg_..."
```

```bash
cargo publish --registry private
```

```toml
[dependencies]
my-crate = { version = "0.1", registry = "private" }
```

A `proxy` repository fronts a sparse index (`upstream = "https://index.crates.io/"`,
or another opencargo's `http://host:port/cargo-hosted/index`); a `group` merges
hosted and proxy members into one index, the first member holding a version
winning. Point cargo at the group and it needs no other registry:

```toml
[registries.all]
index = "sparse+http://registry.example.com/cargo-all/index/"
```

`config.json` is readable without a token even when `anonymous_read = false`,
so cargo learns from `auth-required` to send the token kept in
`$CARGO_HOME/credentials.toml` (cargo also wants
`[registry] global-credential-providers = ["cargo:token"]` in its config for
such a registry). Downloads are fetched from the upstream's `dl`
template, verified against the index checksum and cached; a `dl` pointing at a
private IP literal is refused unless the repository sets `dl_allow_private = true`
(or `OPENCARGO_DL_ALLOW_PRIVATE_<REPO>=1`), which a proxy over a local
opencargo needs. When `upstream_auth` is set, a `dl` off the index host is
refused as well: the credentials go with every upstream request and must not
follow a `dl` elsewhere; list that host in `token_realms` to allow it.

### Docker / OCI

```bash
docker login registry.example.com -u dev1
docker tag myapp:latest registry.example.com/oci-private/team/myapp:latest
docker push registry.example.com/oci-private/team/myapp:latest
docker pull registry.example.com/oci-all/library/alpine:3.20
```

`docker login` credentials are exchanged for a one-hour registry token at
`/v2/token` (anonymous pulls get an anonymous token when `anonymous_read` is
on), which is what Docker up to 28.x needs before it sends credentials on a
push; Basic auth and API tokens keep working directly.

Image names may be nested (`team/myapp`, `org/team/myapp`). A `proxy`
repository fronts another registry (`upstream = "https://registry-1.docker.io"`,
`https://ghcr.io`, or another opencargo as `http://host:6789/oci-hosted`);
one-segment names on Docker Hub get the `library/` prefix automatically. A
`group` lists hosted and proxy members and serves the first one that knows the
image; pushes go to the hosted repository. Manifests, blobs and tag lists are
cached under the proxy member and served on later pulls without upstream
traffic. Upstream credentials stay out of the API:

```toml
[[repositories]]
name = "hub-proxy"
type = "proxy"
format = "oci"
upstream = "https://registry-1.docker.io"
upstream_auth = { type = "basic", username = "hubuser", password = "..." }
```

or `OPENCARGO_UPSTREAM_AUTH_HUB_PROXY=basic:hubuser:...`. The credentials
are only sent to the upstream host and to `token_realms` (Hub's
`https://auth.docker.io/token` is listed by default). Over plain HTTP, add the
host to `insecure-registries` in Docker's `daemon.json`. Use TLS in production.

### Go modules

```bash
export GOPROXY=http://registry.example.com/go-all,direct
export GONOSUMDB=example.com/*
```

`go-all` can be a hosted repository, a proxy (`upstream = "https://proxy.golang.org"`)
or a group whose members are searched in order: `@v/list` is the union of every
member, `@latest` the highest version by semver, `.info`/`.mod`/`.zip` the first
member that has them. Module paths arrive GOPROXY-escaped
(`github.com/!burnt!sushi/toml`) and are unescaped for hosted lookups. An unknown
module answers 404, so `go` moves on to the next `GOPROXY` entry; an unreachable
upstream answers 502, so it stops instead of silently falling back to `direct`.
Canonical versions are cached forever, queries such as `master.info` for ten
minutes. The checksum database is not proxied: exclude private modules with
`GONOSUMDB` or run with `GOSUMDB=off`.

Publish with `PUT /go-private/{module}/@v/{version}` (zip body, raw module
path); see [docs/api.md](docs/api.md).

---

## Configuration

opencargo starts with sane defaults and no config file. Everything below is
optional, and repositories, users, permissions and webhooks are normally
managed through the API or the UI rather than the file. Values marked
`# default: ...` differ from the built-in default.

```toml
[server]
bind = "0.0.0.0:6789"                # default: 127.0.0.1:6789
base_url = "https://registry.example.com"
storage_path = "/data/storage"

[server.tls]                       # optional native TLS (rustls)
cert_path = "/certs/cert.pem"
key_path = "/certs/key.pem"

[database]
url = "sqlite:/data/db/opencargo.db"

[auth]
anonymous_read = true              # set false for a fully private registry
static_tokens = []                 # break-glass admin tokens; keep empty

[auth.admin]
username = "admin"                 # password: OPENCARGO_ADMIN_PASSWORD, or generated

[proxy]
default_ttl = "24h"                # npm packuments, cargo config.json, OCI tags
                                   # (cargo index lines, Go queries and OCI tag lists: 10 min)
negative_cache_ttl = "1h"          # how long an upstream 404 is remembered
connect_timeout = "10s"

[cleanup]                          # optional retention GC
enabled = true                     # default: false
prerelease_older_than_days = 90
proxy_cache_older_than_days = 30   # idle proxy cache entries; swept even with enabled = false
policy_report_older_than_days = 90 # policy report rows; swept even with enabled = false, 0 disables

[vuln_scan]
enabled = true                     # default: false
block_on_critical = false          # refuse a publish with a critical advisory (400)
fail_closed = false                # with block_on_critical: OSV down = 503, not an unscanned publish
osv_base_url = "https://api.osv.dev"
max_concurrency = 8

[policy.npm-proxy]                 # per proxy repository, all rules off by default
min_release_age = "48h"            # Ns | Nm | Nh | Nd
osv_severity = "high"              # low | medium | high | critical; needs vuln_scan.enabled
install_scripts = true             # npm only
typosquat = true                   # not for OCI
fetch_missing_facts = true         # false: cache-only facts, no recorder-initiated upstream request

# Optional seed; managed via API afterwards
[[repositories]]
name = "npm-private"
type = "hosted"
format = "npm"
visibility = "private"

[[repositories]]
name = "hub-proxy"
type = "proxy"
format = "oci"
visibility = "public"
upstream = "https://registry-1.docker.io"
upstream_auth = { type = "basic", username = "hubuser", password = "..." }  # or the env var below
token_realms = ["https://auth.docker.io/token"]  # extra hosts allowed to see the credentials
dl_allow_private = false           # allow a Cargo `dl` / token realm on a private IP (local upstreams)

[[repositories]]
name = "oci-all"
type = "group"
format = "oci"
visibility = "public"
members = ["oci-private", "hub-proxy"]   # same format, resolved in order, nesting up to 5 deep
```

Pass it with `--config /path/config.toml` or `OPENCARGO_CONFIG`. Lookup order
without a flag: `./config.toml`, `~/.opencargo/config.toml`, built-in defaults.

A `[policy.<repo>]` section with at least one rule on records the actor name
(API token name or username), the artifact and the time of every download
through that proxy member for `policy_report_older_than_days` days, and the
admin report (`GET /api/v1/policy/report`) shows what each rule *would* have
blocked;
nothing is blocked, and startup warns which members record. Everyone can see
their own rows at `GET /api/v1/me/policy`. `DELETE /api/v1/policy/report?user=`
erases one user's rows, audited with the count and never the name.

| Variable | Purpose |
|---|---|
| `OPENCARGO_CONFIG` | Path to the config file |
| `OPENCARGO_ADMIN_PASSWORD` | Initial admin password (no generated file, no forced change) |
| `OPENCARGO_BASE_URL` | Public URL of the server, used in tarball and download URLs (also `--base-url`) |
| `OPENCARGO_UPSTREAM_AUTH_<REPO>` | Upstream credentials for a proxy, `basic:user:pass` or `bearer:token`; overrides `upstream_auth`. `<REPO>` is the name uppercased, non-alphanumerics as `_`. Read at startup for every repository, declared in the file or created through the API (restart after creating one) |
| `OPENCARGO_DL_ALLOW_PRIVATE_<REPO>` | `1` to allow that proxy's `dl`/token realm on a private IP (same as `dl_allow_private = true`) |
| `OPENCARGO_OSV_BASE_URL` | OSV API base URL (also `--osv-base-url`) |
| `RUST_LOG` | Log filter, default `opencargo=info,tower_http=info` |

---

## Deployment

**Docker**: see the one-liner above. Pass `--config /config/config.toml`
after the image name to use a mounted config file (it replaces the default
`--bind 0.0.0.0:6789`, so set `server.bind` in the file).

**Kubernetes (Kustomize)**:

```bash
kubectl apply -k k8s/
```

**Helm**:

```bash
helm install opencargo helm/opencargo/ \
  --set auth.adminPassword=change-me \
  --set ingress.enabled=true \
  --set ingress.host=registry.example.com
```

**CI sidecar**: run opencargo next to your runners as a pull-through cache.
Examples for GitHub Actions and GitLab CI in [`k8s/sidecar/`](k8s/sidecar/).

Health: `GET /health/live`, `GET /health/ready`. Metrics: `GET /metrics`.

---

## Verifying a release

Every tag `vX.Y.Z` or `vX.Y.Z-rc.N` publishes, on the GitHub release, static
`x86_64` and `aarch64` musl binaries, a CycloneDX SBOM per binary,
`SHA256SUMS`, and one Sigstore bundle (`<asset>.sigstore.json`) per file. The
container image `ghcr.io/akarasso/opencargo:X.Y.Z[-rc.N]` (plus `X.Y` and `X`
for a final release that is the highest in its line) is the image main CI built and scanned for that commit,
copied by digest, never rebuilt. Everything is signed keyless by GitHub
Actions; the certificate identity names this repository, the workflow file
and the tag, so one command per artifact proves where it came from.

Requires cosign >= v3.0 (tested v3.1.3; v2 cannot read the v3 blob bundles)
and gh >= 2.101.0 with `GH_TOKEN` set (`gh attestation verify` stops at
`gh auth login` otherwise).

```bash
V=0.1.0-rc.1; ISS=https://token.actions.githubusercontent.com
ID=https://github.com/akarasso/opencargo/.github/workflows/release.yml@refs/tags/v$V
gh release download v$V -R akarasso/opencargo && sha256sum -c SHA256SUMS

# Signature of a downloaded file (same for the .cdx.json SBOMs and SHA256SUMS)
cosign verify-blob --bundle opencargo-$V-x86_64-unknown-linux-musl.sigstore.json \
  --certificate-identity "$ID" --certificate-oidc-issuer "$ISS" opencargo-$V-x86_64-unknown-linux-musl

# Build provenance, then the SBOM attestation, of a binary
gh attestation verify opencargo-$V-x86_64-unknown-linux-musl -R akarasso/opencargo --cert-identity "$ID" --cert-oidc-issuer "$ISS"
gh attestation verify opencargo-$V-x86_64-unknown-linux-musl -R akarasso/opencargo \
  --predicate-type https://cyclonedx.org/bom --cert-identity "$ID" --cert-oidc-issuer "$ISS"

# Container image signature, then its two SBOM attestations (Alpine layer, opencargo binary)
cosign verify --new-bundle-format=false ghcr.io/akarasso/opencargo:$V --certificate-identity "$ID" --certificate-oidc-issuer "$ISS"
gh attestation verify oci://ghcr.io/akarasso/opencargo:$V -R akarasso/opencargo \
  --predicate-type https://cyclonedx.org/bom --cert-identity "$ID" --cert-oidc-issuer "$ISS"
```

`--new-bundle-format=false` is required on every image `cosign verify`: GHCR
has no referrers API, so image signatures are stored as classic
`sha256-<digest>.sig` tags, and without the flag cosign v3 accepts any
attestation bundle signed by the same identity as a signature.

Images built on `main` (`sha-<commit>` and `latest`) are signed by `ci.yml`
and carry its build provenance. For a commit `C`:

```bash
C=<full commit sha>; CI=https://github.com/akarasso/opencargo/.github/workflows/ci.yml@refs/heads/main
cosign verify --new-bundle-format=false ghcr.io/akarasso/opencargo:sha-$C \
  --certificate-identity "$CI" --certificate-oidc-issuer "$ISS" --certificate-github-workflow-sha "$C"
gh attestation verify oci://ghcr.io/akarasso/opencargo:sha-$C -R akarasso/opencargo \
  --cert-identity "$CI" --cert-oidc-issuer "$ISS" --source-digest "$C"
```

`latest` can only be checked for identity (drop `--certificate-github-workflow-sha`
and `--source-digest`), not for a given commit. In production, pin the image by
digest. [`scripts/release/verify-release.sh`](scripts/release/verify-release.sh) `<version>`
replays all of the above. Known gap: the SBOMs list Rust crates and Alpine
packages, not the npm packages (`solid-js`, `@solidjs/router`) embedded in the
web UI.

---

## Build from source

```bash
cd frontend && pnpm install && pnpm build && cd ..
cargo build --release
./target/release/opencargo --bind 0.0.0.0:6789
```

`make help` lists the dev targets (`make dev`, `make test`, `make check`,
`make docker`, `tilt up`).

## Tests

```bash
make test-quick     # offline: protocols, proxy/group, OSV against fakes
make test           # everything; e2e suites skip when a client is missing
make test-e2e       # real pnpm, cargo, go and docker clients, required
make test-network   # live npmjs.org and osv.dev (OPENCARGO_NETWORK_TESTS=1)
```

222 integration tests in `tests/` cover the four protocols over HTTP, proxy
and group behaviour against a second opencargo instance and fake upstreams
(Docker Hub token dance, crates.io `dl` templates, a GOPROXY answering 410,
a deterministic OSV), auth, permissions, promotion, webhooks, TLS and the
WebSocket stream. Real-client suites drive `pnpm install`, `cargo publish`
and `cargo fetch`, `go build` and `docker push`/`docker pull` through a
group whose proxy member fronts another instance; locally they print
`skipped:` when the client is absent, and CI runs them with
`OPENCARGO_E2E_REQUIRE=1` so a missing client fails the build.

---

## Documentation

- [docs/api.md](docs/api.md): every HTTP route, the WebSocket protocol, webhook
  payloads and Prometheus metrics.
- [docs/performance.md](docs/performance.md): what one process costs per
  workload, how it was measured, and what was not.
- [README.fr.md](README.fr.md): full French guide.
- [SECURITY.md](SECURITY.md): reporting, scope, hardening checklist.
- [CHANGELOG.md](CHANGELOG.md).

## License

[MIT](LICENSE).
