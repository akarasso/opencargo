# API reference

Base URL: your `server.base_url`. Authentication: `Authorization: Bearer trg_...`
(API tokens) or HTTP Basic (username and password, used by Docker and handy
for curl). Anonymous read is allowed on public repositories when
`auth.anonymous_read = true`.

## npm

```
GET    /{repo}/@{scope}/{name}                     Package metadata
GET    /{repo}/@{scope}/{name}/-/{file}.tgz        Tarball
PUT    /{repo}/@{scope}/{name}                     Publish
GET    /{repo}/-/v1/search?text=                   Search
GET    /{repo}/-/package/@{scope}/{name}/dist-tags  Dist-tags
PUT    /{repo}/-/package/@{scope}/{name}/dist-tags/{tag}
DELETE /{repo}/-/package/@{scope}/{name}/dist-tags/{tag}
PUT    /-/user/org.couchdb.user:{username}         npm login (returns a token)
GET    /-/whoami                                   Current user
```

Unscoped variants drop the `@{scope}/` segment. Reads accept any name npm
still serves (`JSONStream`, `Base64`); publish enforces npm's lowercase rule.
Publishing targets a `hosted` repository. Installing usually goes through a
`group` that lists the hosted repo first and a `proxy` to npmjs.org after.
Through a proxy or a group, metadata and `dist-tags` come from the cached
packument (`proxy.default_ttl`, revalidated with `If-None-Match`), tarballs
are cached forever (a tarball over 100 MiB is refused with `502`) and their
URLs point at the repository the client asked for; `search` covers hosted
members and the packages a proxy member has already served, nested groups
included -- a package this registry has never fetched is not in the index.
`PUT`/`DELETE dist-tags` and publish are `400` on a proxy or a group. An unknown package is `404` and remembered for
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
index lines are cached for ten minutes and revalidated by ETag (its
`config.json` for `proxy.default_ttl`), crates are fetched from the upstream
`config.json`'s `dl` template (`{crate}`, `{version}`, `{prefix}`,
`{lowerprefix}`, `{sha256-checksum}`, default `/{crate}/{version}/download`),
verified against the index `cksum` (`502`, nothing stored, on a mismatch),
capped at 100 MiB and cached forever. A `dl` whose host is, or resolves to,
a private address is refused unless `dl_allow_private`; with `upstream_auth`
a `dl` off the index host is refused unless listed in `token_realms`. Crate
names are matched regardless of case, as cargo lowercases the index path. A
`group` unions the index lines of its members (first member wins on a
version and on download); when a member is unreachable the merged index is
served with `Warning: 199`. Publish, yank and unyank are `400` on a proxy or
a group. Unknown crate: `404`, negative-cached; upstream down: `502`, or the
stale index with `Warning: 110`.

## OCI Distribution v2

```
GET    /v2/
GET    /v2/token?service=opencargo&scope=repository:{repo}/{name}:pull
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

Authentication follows the Docker token model. `GET /v2/` without
credentials, and every `401` on a registry route under `/v2/`, carries
`WWW-Authenticate: Bearer realm="{base_url}/v2/token",service="opencargo"`,
with `scope="repository:{repo}/{name}:pull"` (or `:push`) when the route
names an image. `GET /v2/token` answers `{"token", "access_token",
"expires_in": 3600, "issued_at"}`: with Basic credentials it checks the
user's password (throttled like any Basic login), an API token is accepted
too; without credentials it issues an anonymous token when
`auth.anonymous_read` is on, else `401`. The token endpoint's own `401`
carries no challenge, only the `{"errors": [...]}` body Docker shows.
`scope` may repeat and is only recorded in the token; rights are checked per
request against the user's permissions, so a token never grants more than
its user has, and an anonymous token only what an anonymous caller has. Tokens
are signed with a key generated at startup, expire after one hour and live in
no table; a restart invalidates them and the client fetches a new one on the
next `401`. `Authorization: Basic` and API tokens are still accepted on every
`/v2/` route, so curl and older tooling need no token dance.

`{name}` may span several segments (`team/app`, `org/team/app`). Reads work
on `hosted`, `proxy` and `group` repositories: a proxy fetches from
`upstream` (a registry root such as `https://registry-1.docker.io` or
`https://ghcr.io`, or another opencargo repository as
`http://host:6789/oci-hosted`), answers the upstream's Bearer challenge (a
realm whose host is, or resolves to, a private address is refused unless it
is the upstream's own endpoint, listed in `token_realms` or allowed by
`dl_allow_private`), and caches manifests and blobs (up to 4 GiB) by digest,
tags for `proxy.default_ttl` and tag lists for ten minutes; a group serves
the first member that knows the image and merges `tags/list` (`n`, default
100, and `last` apply to the merged list; a partial page carries
`Link: <...>; rel="next"`).
`Docker-Content-Digest` is always derived from the content. Routes the registry does not implement (`referrers`, anything else under `/v2/`) answer `404` with a JSON error body, which clients such as Docker 29 treat as "no referrers". An unreachable
upstream is `502`; an unknown image is `404`, also when the upstream answers
`401`/`403` after issuing a token (a refusal is asked again on the next
request, an upstream `404` is remembered for `proxy.negative_cache_ttl`).
Push, upload and delete routes accept only
`hosted` repositories (`400` otherwise). Registry tokens are only valid under `/v2/`, are signed with a per-process key (a restart invalidates them and the client fetches a new one on its next 401; a horizontally scaled deployment would need a shared signing key), and a token bought with an API token stops working the moment that API token is revoked. Upstream credentials are configured
in the file or the environment (`upstream_auth`, `token_realms`,
`OPENCARGO_UPSTREAM_AUTH_<REPO>`), never through this API.

## Go modules (GOPROXY)

```
GET    /{repo}/{module}/@v/list
GET    /{repo}/{module}/@latest
GET    /{repo}/{module}/@v/{version}.info
GET    /{repo}/{module}/@v/{version}.mod
GET    /{repo}/{module}/@v/{version}.zip
PUT    /{repo}/{module}/@v/{version}               Publish (zip body)
```

`{module}` and `{version}` arrive GOPROXY-escaped (`github.com/!burnt!sushi/toml`,
`v1.0.0-!r!c1`); the publish route takes the raw path. `.info` `Time` is RFC 3339. A `proxy`
(`upstream = "https://proxy.golang.org"` or another opencargo repository)
caches canonical versions forever (zips up to 512 MiB) and `@v/list`,
`@latest` and non-canonical queries (`master.info`) for ten minutes. A `group` answers `@v/list` with the
union of its members, `@latest` with the highest semver, and `.info`/`.mod`/
`.zip` from the first member that has the version. Unknown module or upstream
`410`: `404` (negative-cached; an empty `@v/list` for a known module is
`200`); upstream down: `502`. The checksum database is not proxied.

## NuGet (v3)

```
GET    /{repo}/v3/index.json                                  Service index
PUT    /{repo}/v3/package                                     Push (multipart, first file = .nupkg); also /api/v2/package
DELETE /{repo}/v3/package/{id}/{version}                      Unlist
POST   /{repo}/v3/package/{id}/{version}                      Relist
GET    /{repo}/v3/flatcontainer/{id}/index.json               Every version, listed or not
GET    /{repo}/v3/flatcontainer/{id}/{version}/{id}.{version}.nupkg
GET    /{repo}/v3/flatcontainer/{id}/{version}/{id}.nuspec
GET    /{repo}/v3/registration/{id}/index.json                Pages inlined up to 128 versions
GET    /{repo}/v3/registration/{id}/{version}.json
GET    /{repo}/v3/registration/{id}/page/{lower}/{upper}.json
GET    /{repo}/v3/search?q=&skip=&take=&prerelease=&semVerLevel=&packageType=
```

Point `dotnet` at `{base_url}/{repo}/v3/index.json`. Ids are case-insensitive and
versions normalized as NuGet does (`1.0`, `1.0.0.0`, `01.0.0` and `1.0.0+meta` are one
version): a second push of any spelling is `409`, an invalid package `400`, a package over
250 MiB `413`; concurrent pushes share a 512 MiB spool, and one that waits more than 30 s for
its share is `503`. A push takes an API token as `X-NuGet-ApiKey` (`dotnet nuget push -k`) or as
the Basic password of `packageSourceCredentials`; every credential presented is verified, one
invalid is `401`, and when the key and the Basic credential name different users the key wins
on push and delete. A key that is neither an API token (`trg_` prefix) nor a static token is not a credential: Azure
Artifacts' convention `-k az` beside Basic credentials pushes with the Basic credential alone,
while a `trg_` key that fails verification is still `401`. The service index is read before the
push, so a private repository needs `packageSourceCredentials` even when `-k` is given.

Delete is an unlist: the version leaves search and stays restorable by exact version. A 401
carries `WWW-Authenticate: Basic`, and an anonymous caller on a group with a member it cannot
read gets a 401 whatever the package, so `dotnet restore` asks for credentials instead of
reporting NU1101; an authenticated caller sees the group as if that member were absent.
A hosted push stores the `.nupkg` and nothing else: the `.nuspec` is read out of it at push,
kept with the version row and served from there, never stored as a file of its own.

A `proxy` takes a v3 service index as `upstream` (`https://api.nuget.org/v3/index.json`). Its
documents are rendered with this server's URLs, except `catalogEntry.@id`. A `.nupkg` is
verified against the sha512 of its registration entry, else of its catalog leaf; a republish
under a new hash is fetched again. Resources on another origin than the upstream's receive no
credentials, and any redirect off the origin asked is refused (`502`). A `group` merges
versions and registrations by version, the first member winning, and search hits by id. A
member down never makes a `404`: the other members and verified cache answer, else `502`.
A merged flat index or registration is kept in memory: while the hosted members' versions do
not change, and for at most 60 s when a proxy member contributed to it. A publish, unlist or
relist shows at once; a new upstream version may take up to that long. Storage or database
unavailable is `503`.

## MCP (servers and agent skills)

```
GET    /{repo}/v0.1/servers?limit&cursor&search&updated_since&version&include_deleted
GET    /{repo}/v0.1/servers/{serverName}/versions?include_deleted
GET    /{repo}/v0.1/servers/{serverName}/versions/{version}     {version} = latest
GET    /{repo}/v0/servers[...]                                  the same three, aliased
POST   /{repo}/v0.1/publish                    server.json, hosted only
POST   /{repo}/v0.1/surfaces                   {name, version, tools, runner?, protocolVersion?}
GET    /{repo}/.claude-plugin/marketplace.json
GET    /{repo}/skills/{name}/{version}/skill.zip
PUT    /{repo}/skills/{name}/{version}/skill.zip
DELETE /{repo}/skills/{name}/{version}/skill.zip
GET    /{repo}/clients/{client}/config.json?npm=&pypi=
```

The catalog is the MCP registry's own read API, so a conforming aggregator
can mirror this one. `limit` is clamped to `1..=100` (default 30), the cursor
is `name:version`, `updated_since` filters on our own clock (never on the
`updatedAt` we re-emit) and implies `include_deleted` unless it is given
explicitly. A server name carries one `/` and is percent-encoded by the
client. Records are served verbatim with one key added,
`_meta["eu.opencargo.registry/mirror"]`: the approval, the gate and its
reason, the surface digests and their source, the endpoint counts, the drift
and the finding counts. The official block is the upstream's, byte for byte;
on a hosted repository opencargo mints it (`publishedAt` kept on a republish,
`isLatest` moved by publication order). A non-`mcp` repository answers `400`,
a private one `401`, a version the gate hides `404`. The catalog answers CORS
preflights, which the VS Code gallery sends.

`client` is `claude-code`, `claude-code-managed-file`, `claude-code-managed`,
`cursor` or `vscode`; see [docs/mcp.md](mcp.md).

```
POST   /api/v1/mcp/{repo}/sync                 {full?}        one run now, with its report
POST   /api/v1/mcp/{repo}/probe                {name?, version?}
GET    /api/v1/mcp/{repo}/servers?state=&q=&limit=            state = all|pending|drifted|blocked|approved|not_observed
GET    /api/v1/mcp/{repo}/evidence?name=&version=             surfaces, findings, decisions, probe runs
POST   /api/v1/mcp/{repo}/approvals            {name, version, state, skill?, note?}
GET    /api/v1/mcp/{repo}/allow-rules
POST   /api/v1/mcp/{repo}/allow-rules          {pattern, effect}
DELETE /api/v1/mcp/{repo}/allow-rules/{id}
GET    /api/v1/mcp/{repo}/suppressions
POST   /api/v1/mcp/{repo}/suppressions         {pattern, tool?}
DELETE /api/v1/mcp/{repo}/suppressions/{id}
```

Admin only, audited. An approval decides every endpoint of the version's
current set in one transaction; a pattern is an exact name or a prefix
ending in `*` after a `/` or a `.`, and the first `allow` rule closes the
repository to everything it does not match. `opencargo mcp sync [--repo R]
[--full]` runs the same code from the command line.

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

GET    /api/v1/policy/report?since=24h&repo=&rule=&page=1&size=50   Policy report (admin)
DELETE /api/v1/policy/report?user=alice | ?user_id=17            Erase one user's rows -> {deleted}
GET    /api/v1/policy/rules                                       Effective rules of every proxy
GET    /api/v1/me/policy?since=&repo=&rule=&page=&size=           The caller's own rows
```

Policy report: a proxy repository with at least one rule enabled in
`[policy.<repo>]` records every artifact it serves (actor, artifact, time)
and evaluates the enabled rules against it; nothing is ever blocked. A member
with no rule on records nothing, so `/rules` says which members record
(`recording`) and echoes each proxy's effective config, defaults included.
`since` is an age (`24h`, `7d`; `s`/`m`/`h`/`d` only, default `24h`, `400`
when it reaches before the earliest representable instant) or an RFC 3339
instant; `repo` matches the requested or the member repository; `rule` is
one of `min_release_age`, `osv_severity`, `install_scripts`, `typosquat`,
`mcp_allowlist`, `mcp_injection`, `mcp_transport`, `mcp_drift`
and narrows every total, chip and verdict to that rule's own row. `totals`
are scoped by the filter; on the admin report alone,
`process.dropped_since_start` is the process-lifetime count of events the
writer dropped under overload. `DELETE`
takes exactly one of `user` (404 when unknown) or `user_id` (works for a
deleted user) and erases by identity, never by label: another user's token
named alike keeps its rows. The erasure is audited as `policy.erase` with
`deleted=N` as its target, never the name. `/me/policy` is forced to the
caller's own rows (a DB user's, every token included, or the config token's)
whatever the query says.

Repository names match `[a-z0-9][a-z0-9._-]{0,63}` without `..`. `type` is
`hosted`, `proxy` (requires `upstream`, an `http(s)` URL) or `group`
(requires a non-empty `members` list of existing repositories of the same
`format`; groups may nest up to 5 levels, cycles are refused); every format
supports the three types. Violations are `400`, on create, update and on
the config seed. `PUT` with another `upstream` purges the proxy's cache
first. `DELETE` on a member of a group is `409`; deleting a proxy also
drops its cache, deleting a group leaves its members' caches alone.
`purge-cache` removes the cached rows and files of a proxy (a group purges
its proxy members) and never touches hosted data. Upstream credentials,
`token_realms` and `dl_allow_private` are set in the config file or the
environment (`OPENCARGO_UPSTREAM_AUTH_<REPO>`, `OPENCARGO_DL_ALLOW_PRIVATE_<REPO>`),
never through this API; the environment is read at startup for every
repository, so a proxy created here takes its credentials at the next
restart (see the README).

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
unscanned. `rescan` needs write access on the repository; an OCI image is
not a package, so both routes answer `404` for one.

## Frontend data

```
GET    /api/v1/dashboard
GET    /api/v1/packages?q=&repo=&page=
GET    /api/v1/packages/{name}
GET    /api/v1/search?q=
```

A search result carries `source`: `hosted` for a package this server holds,
`cached` for one a proxy member served, with the `repository` that answers for
it and `last_seen`. A hosted package wins a name a proxy also serves, and a
cached row has no package page: it is fetched through its repository.

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
| `policy.resolution` | admin | `{repo, member, count, would_block, unknown}`, one per flush per repo pair, at most two a second per pair |
| `mcp.sync` | admin | `{repository, changed, failed}`, one per mirror sync run |
| `mcp.drift` | admin | `{repository, server, version, drift}` when a served version stops matching what was approved |

Client may send `{"type":"ping"}`; server answers `{"type":"pong"}`, pings
every 30 s, re-validates the token every ~5 min (revoked token closes with
code 4401), and sends `{"type":"resync"}` when the client lags.

## Webhooks

Events: `package.published`, `package.promoted`, `*`. With a `secret`, each
delivery carries `X-Webhook-Signature` = HMAC-SHA256 of the body.

## System

```
GET    /health/live
GET    /health/ready             503 {"status":"draining"} during a shutdown
GET    /metrics
GET    /api/v1/system/instance   admin
```

`/api/v1/system/instance` answers `owner` (8 characters), `version`,
`acquired_at`, `renewed_at`, `lease` (`held`, `lost` or `disabled`),
`last_backup_at`, `last_sweep_at`, `last_backup_wal` (`truncated`, `busy` or
null), `incomplete_snapshots`, `shutdown_grace_secs`, `endpoint_drain_secs`
and `open_http_connections`, which excludes WebSocket clients. A lost lease is
never a readiness failure.

Prometheus metrics: `opencargo_http_requests_total{method,path,status}`,
`opencargo_http_request_duration_seconds{method,path}`,
`opencargo_downloads_total{repo,package}` (hosted artifacts served: npm
tarballs, crates, module zips, OCI blobs), `opencargo_publishes_total{repo,package}`,
`opencargo_cache_hits_total{repo}` and `opencargo_cache_misses_total{repo}`
(proxy cache lookups, per member repository; a stale row counts as a miss),
`opencargo_policy_dropped_total` (policy events dropped under overload).
