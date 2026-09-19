# MCP servers and agent skills

opencargo governs what an agent may *install*, not what it may call at
runtime: a private mirror of the MCP registry, an allowlist and an approval
per repository, a fingerprint of every declared permission set and every tool
description, an injection scan with evidence, and the client files a fleet
actually reads. Runtime call gating is a gateway's job and is out of scope.

## The format

`mcp` is a repository format like the others, with the three kinds:

- **proxy** — a mirror of an upstream registry, synced into SQLite by a
  background task (`sync_interval`, default `1h`) rather than fetched per
  request: the allowlist, the drift comparison and the scan all need
  materialised rows and a previous state.
- **hosted** — the registry of record for internal servers and for agent
  skills. opencargo mints the official `_meta` block there.
- **group** — a team's merged view over shared mirrors, with its own allow
  rules, its own approvals and its own suppressions.

A record is stored as the upstream envelope, verbatim; only our own
`_meta["eu.opencargo.registry/mirror"]` key is added when it is served, and
any inbound one is stripped on ingest.

## Routes

The catalog is the registry's own API, so a conforming client can read it:

```
GET  /{repo}/v0.1/servers?limit&cursor&search&updated_since&version&include_deleted
GET  /{repo}/v0.1/servers/{serverName}/versions
GET  /{repo}/v0.1/servers/{serverName}/versions/{version}   ({version} = latest)
POST /{repo}/v0.1/publish      server.json, hosted only
POST /{repo}/v0.1/surfaces     an attested tools/list snapshot
GET  /{repo}/.claude-plugin/marketplace.json
GET|PUT|DELETE /{repo}/skills/{name}/{version}/skill.zip
GET  /{repo}/clients/{client}/config.json?npm=&pypi=
```

`/{repo}/v0/servers…` is the same handler under a second name, and a gallery
base given as an endpoint (`…/v0/servers`) collapses rather than 404s. The
server name is percent-encoded by the client and folded back before routing.
`limit` is clamped to `1..=100` (default 30), the cursor is `name:version`,
and `updated_since` filters on *our* clock, so a downstream aggregator sees
what changed here even when the upstream timestamps are months old.

`client` is `claude-code` (`.mcp.json`), `claude-code-managed-file`
(`managed-mcp.json`), `claude-code-managed` (the managed-settings fragment),
`cursor` or `vscode`. Every file is built from the approved, allowed, latest
versions only. A stdio package is rewritten onto the opencargo repository
named by `?npm=` or `?pypi=`; a private one is skipped with the `.npmrc` line
it needs rather than emitted as a config that 401s. A secret is always
`${VAR}`, never a value.

## What is approved, and against what

Two surfaces are fingerprinted, separately: the **permission surface**
(packages, their arguments, environment variables and transports, remotes,
their variables and headers, icon URLs — values hashed, secrets redacted) and
the **tool surface** (every field of every tool, sorted by name). An approval
binds one *endpoint* — a `remotes[]` URL, or the declared slot for a stdio or
never-observed server — to that pair. A version counts as approved when every
endpoint of its current set is, and a change in either half is a drift:
`permissions`, `tools`, `both`, or `new_endpoint` when a remote nobody
reviewed appears. Tools observed for the first time are a review, not a
silent pass; a lost probe is not a change.

Tool text is obtained honestly or not at all: `probe` (opencargo speaks MCP
to a remote), `attested` (a CI runner posts what it saw a stdio server
answer) or `declared` (the record alone, `tools: not observed` in the UI).
opencargo never runs `npx`, `uvx` or `docker run` to harvest descriptions.

## Modes

`[mcp.<repo>] mode` is `off`, `warn` (the default) or `hide`. Under `warn`
nothing is removed: the record is served unchanged and the reason reaches a
client only through our own `_meta` key and the admin UI. Under `hide` an
unapproved, drifted or unlisted version leaves every listing and answers
`404` on detail — including a version that gained an endpoint, which is a
named review for the admin and a bare `404` for the developer. The allow
rules of every `hide` repository that can reach a member are a floor: a group
in front of a closed mirror narrows it, never widens it.

## The scan

Seven hand-written patterns plus invalid `x-mcp-header` annotations, over
every model-facing string: a record's description and title, each tool's
description, title and annotation title, every description or title at any
depth of `inputSchema` and `outputSchema`, and a skill's frontmatter and
body. `high` patterns gate (`mcp_injection`); `medium` ones are recorded and
shown, and gate only under `scan_medium`. `config_path` is `medium` until a
directive or a parameter sink shares its text, and such a promotion is
recorded: only a *natively* high finding in a skill's frontmatter removes it
from `marketplace.json`. Suppression is a row on the addressed repository,
applied at read time; the finding itself is kept with its span and an escaped
excerpt. `scripts/mcp-corpus.py` rebuilds the false-positive corpus from the
tools the reachable remotes of the committed live page answer, plus a
hand-written benign set.

## Policy rules

`mcp_allowlist`, `mcp_injection`, `mcp_transport` and `mcp_drift` are four
more rules of the policy report, off by default, enabled per member in
`[policy.<repo>]`. One event is recorded per served version — the moment
before a client installs — never per listing. Nothing is blocked.

## Limits

- The VS Code gallery (`chat.mcp.gallery.serviceUrl`, `chat.mcp.access =
  "registry"`) is the one client that refuses what the catalog does not carry.
  Neither the value's shape nor its API version is documented, and no
  automated test can drive a real VS Code: the several spellings served here
  are insurance, **not yet verified against a VS Code build**. That gallery
  repository must also be `public` with `auth.anonymous_read` on, because
  VS Code reads a `401` as "no such API"; it should therefore hold the mirror
  and not internal servers, which reach a fleet through `managed-mcp.json`.
- A developer who edits their own config can add any server, unless
  `managed-mcp.json` is deployed to a system path they cannot write.
- A remote server's tools may change between our probe and a developer's
  session, and may differ by the credentials presented: a fingerprint is a
  point-in-time claim about the anonymous view.
- An internal remote is unprobeable until `probe_allow_private` is set on its
  repository: the SSRF guard resolves every host and refuses private
  addresses by construction, and re-checks every redirect hop.
- The scan finds known shapes; plain prose the model still obeys passes.
- The upstream registry is in preview and may reset its data. Our mirror is
  the durable copy, which is also why a server can vanish upstream for
  reasons we cannot see.
