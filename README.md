# opencargo

**A self-hosted package registry for npm, Cargo, Docker/OCI, Go, PyPI, Maven,
NuGet and raw files, in one binary.**

Host your private packages, proxy and cache the public registries, promote
releases from dev to prod, one binary for the whole team. No JVM, no Postgres,
no telemetry. SQLite inside: 19 MiB of RAM at rest, and 44 MiB at the peak of a
warm `npm install` of 109 packages — [measured](docs/performance.md), not
estimated.

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

opencargo is the fourth option: one static binary that serves the eight formats
a polyglot team actually uses, with the features the "light" options lack
(proxy, group, promotion, per-user permissions, scoped tokens, routing rules)
and without the weight of the enterprise ones.

## What it does today

- **Formats**: npm (scoped packages, dist-tags, `npm login`), Cargo (sparse
  index, yank/unyank), OCI Distribution v2 (Docker push/pull), Go modules
  (GOPROXY), PyPI (PEP 503/691 and the legacy upload), Maven (releases and
  snapshots), NuGet v3, and raw files for everything with no protocol of its
  own.
- **Search over what this server has served**: a proxied package is findable as
  soon as it has been fetched once, in the UI and in `npm search`. A search
  never leaves the process.
- **Routing rules**: a name that must never leave — `@acme/*` — is refused at
  the group boundary instead of being asked upstream. A rule only ever *removes*
  members from a resolution: it can never open a path, only close one. See
  [docs/routing.md](docs/routing.md).
- **Scoped tokens**: a credential carries read-only or read/write, the
  repositories it names, and an expiry. The effective right is the intersection
  with what its bearer may do at that instant, so a token can neither outlive a
  revoked grant nor exceed the role behind it.
- **Repository types**: `hosted` (you publish), `proxy` (transparent cache of
  an upstream) and `group` (one URL in front of several repos, ordered
  resolution), for all eight formats. Metadata is cached for a TTL and
  revalidated with ETags, immutable artifacts (tarballs, crates, module zips,
  blobs) forever; a stale copy is served with `Warning: 110` when the
  upstream is down, and an unreachable upstream is a `502`, never a silent
  `404`.
- **Routing rules** against dependency confusion: `@acme/*` is served by your
  hosted repository or by nobody, whatever the spelling and whichever URL is
  used; a refused member is never asked, and a client gets the same 404 the
  group already answered. See [docs/routing.md](docs/routing.md).
- **Promotion**: move a version from `dev` to `prod` without re-uploading or
  changing lockfiles; full audit trail.
- **Permissions**: roles plus a per-user × per-repository matrix, editable in
  the UI, enforced server-side and on the event stream. API tokens can be
  **scoped** to repositories, packages and actions; a scope only removes, is
  frozen at issue and reaches no administrative route. Configuration tokens
  (`auth.static_tokens`) are operations keys and carry no scope — to scope a
  CI, give it an account and a token. See [docs/api.md](docs/api.md).
- **Dependency graph**: dependencies extracted at publish time; "who depends on
  this?" and impact analysis before you delete a version.
- **Vulnerability scanning** through [OSV.dev](https://osv.dev) on every
  publish once enabled (`[vuln_scan] enabled = true`, off by default), with
  a per-advisory severity (OSV label, else CVSS 3.x/4.0 score) and an
  optional block of critical publishes before anything is written.
- **MCP governance** (preview): an `mcp` repository mirrors the MCP registry,
  hosts internal servers and agent skills, and governs what an agent may
  install — an allowlist and an approval per repository, a fingerprint of
  every declared permission set and every observed tool list with
  re-approval on drift, an injection scan with the offending text, and
  generated `.mcp.json`, `managed-mcp.json`, Cursor and VS Code files built
  from the approved set. See [docs/mcp.md](docs/mcp.md).
- **Webhooks** with HMAC signatures, **WebSocket event stream**, **Prometheus
  metrics**, full-text search, rate limiting, native TLS.
- **Web UI** embedded in the binary: live dashboard, package pages with
  README and install snippets, admin screens, Cmd+K palette.
- **Ops**: Dockerfile, Kustomize manifests, Helm chart, CI sidecar mode for
  caching in GitHub Actions / GitLab CI runners.

Argon2 for passwords, hashed API tokens, path-traversal guards and the
permission matrix are covered by integration tests. See [SECURITY.md](SECURITY.md).

## Migrating in

You are probably already running something. `opencargo import` copies it over,
resumable, without asking the old server to stop:

```bash
opencargo import verdaccio    --from /var/lib/verdaccio/storage --into npm-private
opencargo import nexus        --url https://nexus.example.com --repo npm-hosted --into npm-private
opencargo import artifactory  --url https://artifactory.example.com --repo libs-release --into maven-releases
opencargo import github       --owner acme --into npm-private
opencargo import distribution --url https://registry.example.com --into oci-private
```

The five importers are tested in CI against Verdaccio 6, Nexus OSS 3.76 and
`registry:2`; nothing else has been exercised yet. Each run writes a gap report
of what it could not take. See [docs/import.md](docs/import.md).

## Where it is going

The registry is the foundation. The next layer is a **dependency policy
engine, in audit mode first**: rules versioned with your code
(`package.age < 72h`, `cve.severity >= high`, `install_scripts && !allowlisted`,
`license in [AGPL]`), evaluated at resolution time, scoped per repository so
internal packages are not judged like public ones. The first deliverable is a
weekly report of *what would have been blocked*, before anything is actually
blocked. Governance of MCP servers and agent skills is the second layer and
landed in preview (see above), as did the migration importers (see above).

The registry, audit mode and OIDC SSO are and will stay MIT. Organisation-level
enforcement (quarantine, approvals, audit exports, compliance reports)
is planned as a paid add-on for self-hosted deployments. If you run opencargo
and would talk for 30 minutes about what you would want blocked, open an
issue or write to the address in `SECURITY.md`.

## Comparison

Every competitor cell was read on its vendor's own documentation on
**19 September 2026**; the sources are listed under the table. Editions compared
are the ones you can run yourself without paying: Nexus Repository **Community
Edition**, Artifactory's **non-commercial** edition, Harbor and Verdaccio (both
fully open source), Forgejo/Gitea Packages. A cell reading "paid" names the tier.

| | opencargo | Forgejo / Gitea Packages | Nexus Repository CE | Verdaccio | Harbor | JFrog Artifactory |
|---|---|---|---|---|---|---|
| Formats | 8: npm, Cargo, OCI, Go, PyPI, Maven, NuGet, raw | 24 [11] | ~26, identical in every edition [1] | npm only | OCI, and Helm through OCI | ~55 in the paid tiers; the non-commercial editions are narrow (JCR: Docker, Helm, OCI, generic — OSS: Maven, Gradle, Ivy, SBT, generic) [2] |
| Usage cap | none | none | **40 000 components or 100 000 requests/day**, then new components are refused [3]; Pro raises the cap to 50 000 components / 15 M requests per month, it does not remove it [14] | none | none | none |
| Upstream proxy + cache | all 8 formats | no | yes | yes | yes | yes |
| Group / virtual repos | all 8 formats | no | yes | n/a | no | yes |
| Promotion dev → prod | yes | no | no (staging is Pro [4]) | no | replication | yes, Pro X and up |
| Per-user × per-repo permissions | yes | per forge repo | yes | basic | project-level | yes |
| Scoped tokens (read-only, named repos, expiry) | yes, and the right is the **intersection** with what the bearer may do now, so a token cannot outlive a revoked grant | `read:package` / `write:package`, not per repository [12] | *user tokens*, **Pro** [4] | no | robot accounts, free | yes |
| Search covers proxied packages | yes, what this server has served | no | yes, once cached [15] | hosted only | OCI only | yes |
| Routing rules (block a name from leaving) | yes | no | *routing rules*, CE [5] | no | no | patterns |
| Vulnerability scan | OSV, built in, at publish | no | separate product (Repository Firewall), works with CE [6] | no | Trivy, free | Xray, **Pro X** and up |
| Block at resolution (quarantine) | **no** (audit mode only) | no | Repository Firewall [6] | no | yes, free, by severity | Xray, Pro X and up |
| OIDC SSO | yes, MIT | yes | no (SAML is **Pro** [4]) | third-party auth plugin [13] | yes, free | **Pro X** [2] |
| LDAP / Active Directory | **no** | yes | yes, CE (absent from the Pro-only list) [4] | third-party auth plugin [13] | yes, free | yes, non-commercial [2] |
| Retention / cleanup policies | **no** (proxy cache TTL only) | no | by age and downloads in CE; "keep N versions" is Pro [4] | no | 15 rules per project, free | yes |
| Storage quotas | **no** | no | soft quota per blob store | no | per project, free | yes |
| Tag immutability / version protection | **no** | no | *tags*, Pro [4] | no | yes, free | yes |
| SBOM generation | **no** (opencargo signs its own releases) | no | no | no | Trivy SBOM since 2.11, free | Xray |
| Backup / restore | `opencargo backup` / `restore`, S3 sink | forge backup | filesystem backup; the import/export task is **Pro** [4] | file copy | yes | yes |
| High availability | **no**: one writer, a second instance refuses to start | no | **Pro** [7] | no | free via Helm, but requires external HA Postgres + Redis + RWX/S3 storage [8] | **Enterprise X** [2]; JFrog publishes no self-managed annual price [9] |
| Footprint | one binary, SQLite: **16 MiB RSS idle, ~140 MiB peak on a warm npm install** [10] | part of a forge | JVM, 2 GB+ RAM | Node.js | 8+ containers, Postgres, Redis | JVM, 4 GB+ RAM |
| License | MIT | MIT | EPL + usage caps | MIT | Apache-2.0 | proprietary |

The two rows that matter most against opencargo are **formats** and
**retention**: 26 free formats against 8, and no cleanup policy at all here.
Read [Known limitations](#known-limitations) before the table sells you anything.

Sources, all read on 19 September 2026:
[1] https://help.sonatype.com/en/formats.html ·
[2] https://docs.jfrog.com/installation/docs/feature-comparison-matrix-for-self-mangaged-jpds (JFrog's own spelling) ·
[3] https://help.sonatype.com/en/ce-onboarding.html and https://help.sonatype.com/en/usage-center.html ·
[4] https://help.sonatype.com/en/nexus-repository-pro-features.html and https://help.sonatype.com/en/staging.html ·
[5] https://help.sonatype.com/en/routing-rules.html (no edition restriction stated, and absent from the Pro feature list) ·
[6] https://help.sonatype.com/en/repository-firewall-getting-started.html ·
[7] https://help.sonatype.com/en/high-availability-deployment.html ·
[8] https://goharbor.io/docs/2.15.0/install-config/harbor-ha-helm/ ·
[9] https://jfrog.com/pricing/ — self-managed Pro is monthly, Pro X and Enterprise X are quoted by sales; no annual figure is published ·
[10] measured, [docs/performance.md](docs/performance.md) ·
[11] https://forgejo.org/docs/latest/user/packages/ ·
[12] https://forgejo.org/docs/latest/user/token-scope/ ·
[13] https://verdaccio.org/docs/plugins/ — authentication is a plugin type; the built-in one is htpasswd ·
[14] https://www.sonatype.com/products/pricing ·
[15] https://help.sonatype.com/en/searching-for-components.html — components appear once cached locally, not from the remote directly.

## Known limitations

Read this before the comparison table sells you anything.

- PyPI, Maven, NuGet, raw files (hosted, proxy, group), S3-compatible storage,
  OIDC SSO, MCP governance, routing rules, scoped tokens and search over the
  proxy cache are new: tested in CI, not yet validated on a second deployment.
- MCP: the one client that enforces a catalog today is VS Code
  (`chat.mcp.gallery.serviceUrl` with `chat.mcp.access = "registry"`). Neither
  the setting's value shape nor its API version is documented and no automated
  test can drive a real VS Code, so opencargo serves several spellings and
  **that row is expected, not verified**. Such a gallery repository must be
  `public` with `anonymous_read` on (VS Code reads a `401` as "no such API"),
  which is why it should hold the mirror and not internal servers. Tool
  descriptions exist only where someone observed them: opencargo probes remote
  servers (never private addresses unless the repository opts in) and takes
  attested snapshots for stdio servers, and never runs a package to harvest
  them.
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
- One instance per database: a second process on the same database refuses
  to start, and every upgrade has a downtime window (`Recreate`). Read scale
  is other opencargo instances proxying this one, each with its own cache
  and upstream traffic. See [docs/operations.md](docs/operations.md).
- Vulnerability severity is read per advisory from the full OSV record: a
  `database_specific.severity` label wins, else the highest CVSS 3.x/4.0
  vector is scored, and `MAL-` ids are critical. `vuln_scan.block_on_critical`
  refuses such a publish before anything is written; `vuln_scan.fail_closed`
  turns an OSV outage into a 503 instead of an unscanned publish. Advisories
  with only CVSS 2 data (or none) are reported as `unknown` and never block.
- No retention or cleanup policy beyond the proxy cache TTL, no storage quota,
  no tag immutability, no LDAP, no SAML, no SBOM generation, and no signature
  verification of what is ingested. Each of these is designed or on the list;
  none of them exists today.
- Blocking is at publish time only. The policy engine runs in audit mode, so a
  vulnerable package already cached is still served.
- One maintainer, pre-1.0. Pin the image by digest and back up with
  `opencargo backup` ([docs/operations.md](docs/operations.md)).

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
cached packument); `npm search` and the UI cover hosted members and whatever
the proxy members have already served, nested groups included. `npm publish` and
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

### Python / PyPI

```ini
# ~/.config/pip/pip.conf
[global]
index-url = https://__token__:trg_...@registry.example.com/pypi-all/simple/
```

```bash
twine upload --repository-url https://registry.example.com/pypi-private/legacy/ \
  -u __token__ -p trg_... dist/*
uv pip install --index-url "https://__token__:trg_...@registry.example.com/pypi-all/simple/" demo
```

The Basic username is the literal `__token__` and the password is an API token.
A `proxy` takes an index base as `upstream` (`https://pypi.org/simple`); files
are fetched only from `files.pythonhosted.org`, the index's own host, or the
repository's `file_hosts` list, and verified against the sha256 the page
announced. `GET /{repo}/simple/` enumerates hosted members only — an upstream
index is never walked. See [docs/pypi.md](docs/pypi.md).

### Maven / Gradle

```xml
<settings>
  <servers>
    <server><id>oc</id><username>dev1</username><password>trg_...</password></server>
  </servers>
</settings>
```

```xml
<repositories>
  <repository><id>oc</id><url>https://registry.example.com/maven/maven-all/</url>
    <releases><enabled>true</enabled></releases>
    <snapshots><enabled>true</enabled></snapshots>
  </repository>
</repositories>
<distributionManagement>
  <repository><id>oc</id><url>https://registry.example.com/maven/maven-releases/</url></repository>
  <snapshotRepository><id>oc</id><url>https://registry.example.com/maven/maven-snapshots/</url></snapshotRepository>
</distributionManagement>
```

Gradle takes the same URLs in `maven { url = uri("...") ; credentials { ... } }`.
Deploy is `mvn deploy`, read is `GET /maven/{repo}/{group path}/{artifact}/{version}/{file}`;
`maven-metadata.xml` is generated for hosted members and merged across a group.
A `proxy` takes a repository base as `upstream` (`https://repo1.maven.org/maven2`).

### NuGet / .NET

```xml
<configuration>
  <packageSources>
    <add key="oc" value="https://registry.example.com/nuget-all/v3/index.json" />
  </packageSources>
  <packageSourceCredentials>
    <oc><add key="Username" value="dev1" /><add key="ClearTextPassword" value="trg_..." /></oc>
  </packageSourceCredentials>
</configuration>
```

```bash
dotnet nuget push Greeter.1.0.0.nupkg --source oc --api-key trg_...
dotnet restore
```

The service index is read before a push, so a private repository needs
`packageSourceCredentials` even when `--api-key` is given. A key that is
neither an API token nor a static token is not treated as a credential, so
Azure Artifacts' `-k az` beside Basic credentials pushes with the Basic
credential alone. Delete is an unlist: the version leaves search and stays
restorable by exact version.

### Raw / generic files

```bash
curl -u dev1:trg_... -T ./toolchain.tar.gz \
  https://registry.example.com/raw/raw-private/dist/toolchain.tar.gz
curl -O https://registry.example.com/raw/raw-all/dist/toolchain.tar.gz
```

Anything with no protocol of its own: toolchains, firmware, build artifacts.
A `proxy` mirrors a static file tree under its `upstream`, a `group` serves the
first member holding the path. `GET /api/v1/raw/{repo}/files?prefix=` lists what
a repository holds, 50 entries per page.

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

lease_wait = "60s"                 # writer lease, shutdown: docs/operations.md
shutdown_grace = "30s"
endpoint_drain = "0s"

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

[limits.publish]                   # docs/operations.md
window = "1m"
per_window = 500                   # default: unset, only the formats below are metered

[limits.publish.format]            # default: npm = 30, pypi = 30
npm = 500
cargo = { max = 1000, per = "5m" }

[limits.publish.repository]        # wins over the format entry, for that repository
npm-ci = { max = 2000, per = "1h" }

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

[backup]                           # docs/operations.md
enabled = true                     # default: false
every = "24h"                      # at t = at (mod every), UTC
at = "03:00"
keep = 7
to = "/backups"
storage = false                    # the schedule copies the database only

[vuln_scan]
enabled = true                     # default: false
block_on_critical = false          # refuse a publish with a critical advisory (400)
fail_closed = false                # with block_on_critical: OSV down = 503, not an unscanned publish
osv_base_url = "https://api.osv.dev"
max_concurrency = 8

[routing]                          # group routing rules; see docs/routing.md
refresh_secs = 30
max_snapshot_age_secs = 300        # past it, the proxy members a rule speaks for are refused
refusal_window_secs = 3600         # how long one refused (name, repo, member) stays deduplicated

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

Publishing is metered per account in a sliding window, and a refused publish
answers `429` with `Retry-After` and the limit it hit. The shipped limits are
thirty npm publishes and thirty PyPI uploads a minute per account, what
opencargo has always enforced; Cargo, Go, NuGet, MCP, raw and OCI (at the
manifest put, so a count is an image count) are metered only once configured,
and a Maven deploy is never metered here, since a deploy is a file per request
with none that completes it. A limit is always finite -- `0` is refused at
startup -- so it is raised, never lifted. Resolution and the CI recipe are in
[docs/operations.md](docs/operations.md).

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
| `OPENCARGO_LEASE_WAIT`, `OPENCARGO_SHUTDOWN_GRACE`, `OPENCARGO_ENDPOINT_DRAIN` | Override `[server]`; the Helm chart sets them from its values |
| `RUST_LOG` | Log filter, default `opencargo=info`. Colour follows the terminal, so a redirected log is plain text |

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

Health: `GET /health/live`, `GET /health/ready` (`503 draining` during a
shutdown). Metrics: `GET /metrics`. Backups, restore and the upgrade window:
[docs/operations.md](docs/operations.md).

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
- [docs/maven.md](docs/maven.md): the Maven layout, how a version is assembled
  from the files a deploy sends, and what a group merges.
- [docs/pypi.md](docs/pypi.md): the Simple API, the upload route and what a
  proxy verifies.
- [docs/mcp.md](docs/mcp.md): MCP servers and agent skills — the mirror, the
  approvals, the scan and the client files.
- [docs/import.md](docs/import.md): `opencargo import`, copying Nexus,
  Artifactory, Verdaccio, GitHub Packages or any OCI registry into opencargo,
  and the gap report.
- [docs/operations.md](docs/operations.md): one instance, the writer lease,
  shutdown and upgrades, backups and the restore drill.
- [docs/write-amplification.md](docs/write-amplification.md): what a publish
  writes beyond what it keeps, measured, and what the database does about it.
- [docs/performance.md](docs/performance.md): what one process costs per
  workload, how it was measured, and what was not.
- [docs/routing.md](docs/routing.md): routing rules, the spellings they cover,
  and what a refusal does and does not say.
- [README.fr.md](README.fr.md): full French guide.
- [SECURITY.md](SECURITY.md): reporting, scope, hardening checklist.
- [CHANGELOG.md](CHANGELOG.md).

## License

[MIT](LICENSE).
