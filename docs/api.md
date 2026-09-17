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

## Cargo (sparse index)

```
GET    /{repo}/index/config.json
GET    /{repo}/index/{prefix}/{name}
PUT    /{repo}/api/v1/crates/new                   Publish
GET    /{repo}/api/v1/crates/{name}/{ver}/download
DELETE /{repo}/api/v1/crates/{name}/{ver}/yank
PUT    /{repo}/api/v1/crates/{name}/{ver}/unyank
```

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
`401`/`403` after issuing a token. Push, upload and delete routes accept only
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

## Administration

```
POST   /api/v1/repositories                        {name, type, format, visibility, upstream?, members?}
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
POST   /api/v1/vulns/@{scope}/{name}/{version}/rescan
```

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
`opencargo_downloads_total{repo,package}`, `opencargo_publishes_total{repo,package}`,
`opencargo_cache_hits_total{repo}`, `opencargo_cache_misses_total{repo}`,
`opencargo_storage_bytes{repo}`.
