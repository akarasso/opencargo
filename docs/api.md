# API reference

Base URL: your `server.base_url`. Authentication: `Authorization: Bearer trg_...`
(API tokens) or HTTP Basic (username and password, used by Docker and handy
for curl). Anonymous read is allowed on public repositories when
`auth.anonymous_read = true`.

## npm

```
GET    /{repo}/{name}                              Package metadata
GET    /{repo}/@{scope}/{name}                     Package metadata (scoped)
GET    /{repo}/@{scope}/{name}/-/{file}.tgz        Tarball
PUT    /{repo}/@{scope}/{name}                     Publish
GET    /{repo}/-/v1/search?text=                   Search
GET    /{repo}/-/package/@{scope}/{name}/dist-tags  Dist-tags
PUT    /{repo}/-/package/@{scope}/{name}/dist-tags/{tag}
DELETE /{repo}/-/package/@{scope}/{name}/dist-tags/{tag}
PUT    /-/user/org.couchdb.user:{username}         npm login (returns a token)
GET    /-/whoami                                   Current user
```

Publishing targets a `hosted` repository. Installing usually goes through a
`group` that lists the hosted repo first and a `proxy` to npmjs.org after.
Through a proxy or a group, metadata and `dist-tags` come from the cached
packument (`proxy.default_ttl`, revalidated with `If-None-Match`), tarballs
are cached forever and their URLs point at the repository the client asked
for; `search` walks nested groups. `PUT`/`DELETE dist-tags` and publish are
`400` on a proxy or a group. An unknown package is `404` and remembered for
`proxy.negative_cache_ttl`; an unreachable upstream is `502`, or the stale
cached copy with `Warning: 110`.

## Cargo (sparse index)

```
GET    /{repo}/index/config.json
GET    /{repo}/index/{prefix}/{name}
PUT    /{repo}/api/v1/crates/new                   Publish
GET    /{repo}/api/v1/crates/{name}/{ver}/download
DELETE /{repo}/api/v1/crates/{name}/{ver}/yank
PUT    /{repo}/api/v1/crates/{name}/{ver}/unyank
```

`config.json` is served without a token even when `auth.anonymous_read =
false` and carries `"auth-required": true` on a private repository, so cargo
sends the token from `$CARGO_HOME/credentials.toml`; its `dl` always points at
the requested repository. A `proxy` has `upstream` = a sparse index root
(`https://index.crates.io/` or another opencargo's `.../cargo-hosted/index`):
index lines are cached for `proxy.default_ttl` and revalidated by ETag,
crates are fetched from the upstream `config.json`'s `dl` template
(`{crate}`, `{version}`, `{prefix}`, `{lowerprefix}`, `{sha256-checksum}`,
default `/{crate}/{version}/download`), verified against the index `cksum`
(`502`, nothing stored, on a mismatch) and cached forever. A `dl` on a
private IP literal is refused unless `dl_allow_private`; with `upstream_auth`
a `dl` off the index host is refused unless listed in `token_realms`. A
`group` unions the index lines of its members (first member wins on a
version and on download); when a member is unreachable the merged index is
served with `Warning: 199`. Publish, yank and unyank are `400` on a proxy or
a group. Unknown crate: `404`, negative-cached; upstream down: `502`, or the
stale index with `Warning: 110`.

## OCI Distribution v2

```
GET    /v2/
HEAD   /v2/{repo}/{name}/blobs/{digest}
GET    /v2/{repo}/{name}/blobs/{digest}
DELETE /v2/{repo}/{name}/blobs/{digest}
POST   /v2/{repo}/{name}/blobs/uploads/
PATCH  /v2/{repo}/{name}/blobs/uploads/{uuid}
PUT    /v2/{repo}/{name}/blobs/uploads/{uuid}?digest=
GET    /v2/{repo}/{name}/manifests/{reference}
HEAD   /v2/{repo}/{name}/manifests/{reference}
PUT    /v2/{repo}/{name}/manifests/{reference}
DELETE /v2/{repo}/{name}/manifests/{reference}
GET    /v2/{repo}/{name}/tags/list
```

`{name}` may span several segments (`team/app`, `org/team/app`). Reads work
on `hosted`, `proxy` and `group` repositories: a proxy fetches from
`upstream` (a registry root such as `https://registry-1.docker.io` or
`https://ghcr.io`, or another opencargo repository as
`http://host:6789/oci-hosted`), answers the upstream's Bearer challenge, and
caches manifests and blobs by digest, tags for `proxy.default_ttl` and tag
lists for ten minutes; a group serves the first member that knows the image
and merges `tags/list` (`n` and `last` apply to the merged list).
`Docker-Content-Digest` is always derived from the content. An unreachable
upstream is `502`; an unknown image is `404`, also when the upstream answers
`401`/`403` after issuing a token (a refusal is asked again on the next
request, an upstream `404` is remembered for `proxy.negative_cache_ttl`).
Push, upload and delete routes accept only
`hosted` repositories (`400` otherwise). Upstream credentials are configured
in the file or the environment (`upstream_auth`, `token_realms`,
`OPENCARGO_UPSTREAM_AUTH_<REPO>`), never through this API.

## Go modules (GOPROXY)

```
GET    /{repo}/{module}/@v/list
GET    /{repo}/{module}/@v/{version}.info
GET    /{repo}/{module}/@v/{version}.mod
GET    /{repo}/{module}/@v/{version}.zip
PUT    /{repo}/{module}/@v/{version}               Publish (zip body)
```

`{module}` and `{version}` arrive GOPROXY-escaped (`github.com/!burnt!sushi/toml`,
`v1.0.0-!r!c1`); the publish route takes the raw path. `.info` `Time` is RFC 3339. A `proxy`
(`upstream = "https://proxy.golang.org"` or another opencargo repository)
caches canonical versions forever and `@v/list`, `@latest` and non-canonical
queries (`master.info`) for ten minutes. A `group` answers `@v/list` with the
union of its members, `@latest` with the highest semver, and `.info`/`.mod`/
`.zip` from the first member that has the version. Unknown module or upstream
`410`: `404` (negative-cached; an empty `@v/list` for a known module is
`200`); upstream down: `502`. The checksum database is not proxied.

## Administration

```
POST   /api/v1/repositories                        {name, type, format, visibility, upstream?, members?, dl_allow_private?, token_realms?}
GET    /api/v1/repositories
GET    /api/v1/repositories/{name}
PUT    /api/v1/repositories/{name}
DELETE /api/v1/repositories/{name}
POST   /api/v1/repositories/{name}/purge-cache

POST   /api/v1/users                               {username, email?, role}  -> one-time password
GET    /api/v1/users
GET    /api/v1/users/{username}
PUT    /api/v1/users/{username}
DELETE /api/v1/users/{username}
PUT    /api/v1/users/{username}/password           {current_password, new_password}
GET    /api/v1/users/{username}/tokens
POST   /api/v1/users/{username}/tokens             {name, expires_in_days}  -> one-time token
DELETE /api/v1/users/{username}/tokens/{id}
GET    /api/v1/users/{username}/permissions
PUT    /api/v1/users/{username}/permissions/{repo} {can_read, can_write, can_delete, can_admin}
DELETE /api/v1/users/{username}/permissions/{repo}
GET    /api/v1/me/permissions                      Effective rights of the caller, with their source

GET    /api/v1/webhooks
POST   /api/v1/webhooks                            {url, events, secret?}
PUT    /api/v1/webhooks/{id}
DELETE /api/v1/webhooks/{id}
POST   /api/v1/webhooks/{id}/test

GET    /api/v1/system/audit?page=1&size=50
```

Repository names match `[a-z0-9][a-z0-9._-]{0,63}` without `..`. `type` is
`hosted`, `proxy` (requires `upstream`, an `http(s)` URL) or `group`
(requires a non-empty `members` list of existing repositories of the same
`format`; groups may nest up to 5 levels, cycles are refused); every format
supports the three types. Violations are `400`, on create, update and on
the config seed. `DELETE` on a member of a group is `409`; deleting a proxy
or a group also drops its cache. `purge-cache` removes the cached rows and
files of a proxy (a group purges its proxy members) and never touches
hosted data. Upstream credentials are set in the config file or the
environment, never through this API (see the README).

Roles: `admin` (everything), `publisher` (read + write), `reader` (read).
A per-user, per-repository grant overrides the role. Resolution order:
admin role, explicit grant, role default, anonymous read on public repos.

## Promotion

```
POST   /api/v1/promote/@{scope}/{name}/{version}   {from, to}
POST   /api/v1/promote/{name}/{version}
GET    /api/v1/promotions/@{scope}/{name}/{version}
GET    /api/v1/promotions/{name}/{version}
```

The artifact is not copied; both repositories point to the same file.

## Dependency graph

```
GET    /api/v1/deps/@{scope}/{name}/dependencies
GET    /api/v1/deps/@{scope}/{name}/dependents
GET    /api/v1/deps/@{scope}/{name}/versions/{ver}/impact
```

Unscoped variants drop the `@{scope}/` segment. Dependencies are extracted at
publish time (npm `dependencies`/`devDependencies`, Cargo deps, `go.mod`).

## Vulnerabilities (OSV.dev)

```
GET    /api/v1/vulns/@{scope}/{name}/{version}
POST   /api/v1/vulns/@{scope}/{name}/{version}/rescan     (authenticated)
```

Unscoped variants drop the `@{scope}/` segment. Response:

```json
{"package": "left-pad", "version": "1.3.0", "scanned_at": "...",
 "total_deps": 12, "vulnerable_deps": 1, "status": "critical",
 "details": [{"dependency": "minimist", "version": "1.2.0",
              "vuln_id": "GHSA-...", "summary": "...",
              "severity": "critical", "score": 9.8}]}
```

`status` is `not_scanned`, `clean`, `warning` or `critical`. `severity` is
per advisory: the OSV `database_specific.severity` label when present, else
the highest CVSS 3.x/4.0 vector scored (`critical` ≥ 9.0, `high` ≥ 7.0,
`medium` ≥ 4.0, `low`), `critical` for `MAL-` ids, `unknown` when only CVSS 2
or no data exists (`score` null). Each advisory is fetched once and cached
for the process. With `vuln_scan.block_on_critical`, a publish carrying a
critical advisory is refused with `400` before anything is stored; with
`fail_closed` too, an OSV outage answers `503` instead of publishing
unscanned. `rescan` needs write access on the repository and is `400` for
OCI (no OSV ecosystem).

## Frontend data

```
GET    /api/v1/dashboard
GET    /api/v1/packages?q=&repo=&page=
GET    /api/v1/packages/{name}
GET    /api/v1/search?q=
```

## WebSocket events

`GET /api/v1/events/ws`. Authenticate with the first frame:

```
→ {"type":"auth","token":"trg_..."}          or {"type":"auth"} for anonymous
← {"type":"hello","username":"dev1","role":"publisher","anonymous":false}
← {"type":"package.published","data":{...},"ts":"..."}
```

| Event | Visible to | Payload |
|---|---|---|
| `package.published`, `package.promoted` (public repo) | everyone | full |
| `package.published`, `package.promoted` (private repo) | admin | full |
| `registry.changed` | authenticated | `{repository}` |
| `repositories.changed` | everyone | empty |
| `permissions.changed` | authenticated | `{username}` |
| `audit.entry` | admin | `{username, action, target}` |

Client may send `{"type":"ping"}`; server answers `{"type":"pong"}`, pings
every 30 s, re-validates the token every ~5 min (revoked token closes with
code 4401), and sends `{"type":"resync"}` when the client lags.

## Webhooks

Events: `package.published`, `package.promoted`, `*`. With a `secret`, each
delivery carries `X-Webhook-Signature` = HMAC-SHA256 of the body.

## System

```
GET    /health/live
GET    /health/ready
GET    /metrics
```

Prometheus metrics: `opencargo_http_requests_total{method,path,status}`,
`opencargo_http_request_duration_seconds{method,path}`,
`opencargo_downloads_total{repo,package}` (hosted artifacts served: npm
tarballs, crates, module zips, OCI blobs), `opencargo_publishes_total{repo,package}`,
`opencargo_cache_hits_total{repo}` and `opencargo_cache_misses_total{repo}`
(proxy cache lookups, per member repository; a stale row counts as a miss).
