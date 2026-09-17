# opencargo

**A self-hosted package registry for npm, Cargo, Docker/OCI and Go modules, in one 10 MB binary.**

Host your private packages, proxy and cache npmjs.org, promote releases from
dev to prod, one binary for the whole team.
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
  resolution). Proxy and group exist for npm today; Cargo, Go and OCI are
  hosted only.
- **Promotion**: move a version from `dev` to `prod` without re-uploading or
  changing lockfiles; full audit trail.
- **Permissions**: roles plus a per-user × per-repository matrix, editable in
  the UI, enforced server-side and on the event stream.
- **Dependency graph**: dependencies extracted at publish time; "who depends on
  this?" and impact analysis before you delete a version.
- **Vulnerability scanning** through [OSV.dev](https://osv.dev) on every
  publish (advisory IDs today; severity-based blocking is being fixed, see
  known limitations).
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
blocked. Then PyPI, migration importers from Nexus / Artifactory / Verdaccio /
GitHub Packages, and governance of MCP servers and agent skills distributed
through npm, PyPI and OCI.

The registry and audit mode are and will stay MIT. Organisation-level
enforcement (quarantine, approvals, SSO, audit exports, compliance reports)
is planned as a paid add-on for self-hosted deployments. If you run opencargo
and would talk for 30 minutes about what you would want blocked, open an
issue or write to the address in `SECURITY.md`.

## Comparison

| | opencargo | Forgejo / Gitea Packages | Nexus Repository CE | Verdaccio | Harbor | JFrog Artifactory |
|---|---|---|---|---|---|---|
| Formats | npm, Cargo, OCI, Go | 20+ | 15+ | npm only | OCI, Helm | 30+ |
| Upstream proxy + cache | npm (Cargo, Go, OCI planned) | no | yes | yes | yes | yes |
| Group / virtual repos | npm (others planned) | no | yes | n/a | no | yes |
| Promotion dev → prod | yes | no | paid | no | replication | yes |
| Per-user × per-repo permissions | yes | per forge repo | yes | basic | project-level | yes |
| Vulnerability scan | OSV, built in | no | paid (Firewall) | no | Trivy | paid (Xray) |
| Footprint | 1 binary, SQLite, ~20 MB RAM | part of a forge | JVM, 2 GB+ RAM | Node.js | 8+ containers, Postgres, Redis | JVM, 4 GB+ RAM |
| License | MIT | MIT | EPL, usage caps | MIT | Apache-2.0 | proprietary |

## Known limitations

Read this before the comparison table sells you anything.

- Upstream proxying, caching and groups work for npm only. Cargo, Go and OCI
  repositories are `hosted` today, and the API refuses to create a proxy or a
  group in those formats; pull-through proxies for crates.io, the Go module
  proxy and Docker Hub are the next items on the roadmap.
- OCI image names are a single path segment: `registry/oci-private/app`
  works, `registry/oci-private/team/app` does not yet.
- No PyPI, Maven or NuGet. If you need those today, Forgejo Packages or Nexus
  are better choices.
- Storage is local disk (a volume or PVC), no S3 backend yet. No SSO; users
  and tokens are local.
- `vuln_scan.block_on_critical` does not work yet: the OSV batch API returns
  advisory IDs without severity, so nothing is ever classified critical. A fix
  is in progress; until then treat the scan as an inventory, not a gate.
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

A `group` such as `npm-all` answers `npm install`, `npm dist-tag ls` and
`npm search` from its hosted members first, then from its proxies (dist-tags
come from the cached packument, search walks nested groups); `npm publish` and
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

### Docker / OCI

```bash
docker login registry.example.com -u dev1
docker tag myapp:latest registry.example.com/oci-private/myapp:latest
docker push registry.example.com/oci-private/myapp:latest
```

Over plain HTTP, add the host to `insecure-registries` in Docker's
`daemon.json`. Use TLS in production.

### Go modules

```bash
export GOPROXY=http://registry.example.com/go-private,direct
export GONOSUMCHECK=example.com/*
```

Publish with `PUT /go-private/{module}/@v/{version}` (zip body); see
[docs/api.md](docs/api.md).

---

## Configuration

opencargo starts with sane defaults and no config file. Everything below is
optional, and repositories, users, permissions and webhooks are normally
managed through the API or the UI rather than the file.

```toml
[server]
bind = "0.0.0.0:6789"
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
default_ttl = "24h"
negative_cache_ttl = "1h"

[cleanup]                          # optional retention GC
enabled = true
prerelease_older_than_days = 90
proxy_cache_older_than_days = 180

[vuln_scan]
enabled = true
block_on_critical = false

# Optional seed; managed via API afterwards
[[repositories]]
name = "npm-private"
type = "hosted"
format = "npm"
visibility = "private"
```

Pass it with `--config /path/config.toml` or `OPENCARGO_CONFIG`. Lookup order
without a flag: `./config.toml`, `~/.opencargo/config.toml`, built-in defaults.

| Variable | Purpose |
|---|---|
| `OPENCARGO_CONFIG` | Path to the config file |
| `OPENCARGO_ADMIN_PASSWORD` | Initial admin password (no generated file, no forced change) |
| `OPENCARGO_BASE_URL` | Public URL of the server, used in tarball and download URLs (also `--base-url`) |
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
make test-quick     # no network
make test           # everything, including proxy and OSV tests
```

116 integration tests in `tests/` cover the four protocols over HTTP (npm
also through a real `pnpm` client), auth, permissions, promotion, webhooks,
TLS and the WebSocket stream.

---

## Documentation

- [docs/api.md](docs/api.md): every HTTP route, the WebSocket protocol, webhook
  payloads and Prometheus metrics.
- [README.fr.md](README.fr.md): full French guide.
- [SECURITY.md](SECURITY.md): reporting, scope, hardening checklist.
- [CHANGELOG.md](CHANGELOG.md).

## License

[MIT](LICENSE).
