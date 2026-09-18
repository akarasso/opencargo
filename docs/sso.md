# Single sign-on (OpenID Connect)

opencargo signs users in through any OpenID Provider with the authorization
code flow, PKCE S256, a nonce and `response_mode=query`. MIT, like the rest of
the server: no feature is reserved.

## Configuration

```toml
[auth.sso]
password_mode = "enabled"      # enabled | admins_only | disabled
session_ttl = "12h"            # lifetime of the token a login hands the web UI
reauth_after = "30d"           # empty: never; an SSO user's tokens stop working after this without a fresh login
reauth_grace_max = "72h"       # cap on how long an unreachable provider suspends that clock
handoff_ttl = "120s"
probe_interval = "60s"
dev_insecure_http = false

[[auth.sso.providers]]
name = "corp"
type = "generic"               # google | entra | gitlab | generic
issuer = "https://idp.example.com"
client_id = "opencargo"
client_secret = "..."
scopes = ["groups"]
groups_claim = "groups"        # generic only
open = false                   # generic only: anyone on the internet may hold an account there
required_groups = ["dev"]
allowed_domains = []
authoritative_domains = ["example.com"]
default_role = "reader"        # reader | publisher, never admin
grants = [{ group = "dev", repository = "npm-private", role = "publisher" }]
```

The redirect URI to register at the provider is
`{base_url}/api/v1/auth/sso/{name}/callback`.

| type | issuer | groups | open |
|---|---|---|---|
| `google` | fixed | none; restrict with `allowed_domains` | yes |
| `entra` | `https://login.microsoftonline.com/{tid}/v2.0`, `tenant` = id, `common` or `organizations` | `groups`; overage refuses the login | unless the tenant is pinned |
| `gitlab` | instance URL, gitlab.com by default | `groups_direct` | gitlab.com only |
| `generic` | declared | `groups_claim` | `open`, default true |

An open issuer is refused at startup unless a restrictive rule
(`required_groups`, `allowed_domains`, a pinned Entra tenant) or
`allow_open = true` is set; the opt-in is logged at every start. Two providers
may not declare one issuer. `grants` apply only to private repositories.

SSO needs an `https` `base_url`: the attempt cookie is `__Host-` and
`Secure`. `dev_insecure_http` drops both for a server listening on loopback
with an `http` URL, and is refused otherwise.

## Accounts

- An unknown identity gets a new account named after `preferred_username`
  (else the e-mail's local part). A local account already holding that name
  is never taken over: the login is refused and audited.
- A local account is linked only from its own session: current password,
  then the provider, then a confirmation screen showing `iss`, `sub` and the
  e-mail. Nothing is linked without that click. An e-mail is never a key; it
  is only shown as the reason for a link when it is verified and the provider
  is listed in `authoritative_domains` for its domain (Entra: only with a
  pinned tenant). Admin accounts and the bootstrap account are never linked.
- The role and grants of an account SSO created follow the provider's rules at
  every login; an admin cannot change them. Unlinking restores the role the
  account had before.
- Groups are re-read at every login. Unreadable groups change nothing; a
  denial by the rules disables the account and revokes its SSO credentials.

## Credentials

A login ends with an ordinary API token that carries its provenance, handed to
the web UI through a single-use code bound to the attempt's cookie and
exchanged by a JSON `POST`. It works everywhere an API token does, including as
the Basic password of `docker login` and cargo.

| credential | provider retired / account disabled | other bound |
|---|---|---|
| SSO web token | revoked at once | `session_ttl`; refused at next denied login |
| API token of an SSO user | revoked at once / refused at once | `reauth_after` |
| local password of a linked user | refused at once when disabled | `reauth_after`; `password_mode` |
| OCI `/v2/token` | refused at once: re-checked on every request | its own lifetime |

`reauth_after` only counts time during which the server's own probe
(discovery and keys, every `probe_interval`) could reach the provider; a
client can never feed it. An outage suspends it for at most
`reauth_grace_max`.

## Removing or moving a provider

A provider the database knows and the configuration no longer names refuses
the start, naming it. Declare it instead:

- `retired = true` on its entry: every credential it produced is revoked and
  its links disabled, before the server listens. Reintroducing it restores
  nothing.
- `issuer_was = "https://old.example"` on its new entry: links and provenance
  move to the new issuer.

## Known limits

- The web UI keeps its token in `localStorage`, exposed to an XSS; the CSP,
  `session_ttl` and revocation bound it.
- The audit of forged callbacks is capped per node and in memory; a restart
  or several replicas relax the cap.
- During a rolling deployment, an old pod accepts a retired provider's
  credentials until it stops.
- The e-mail domain rule only proposes a link; a wrongly declared
  `authoritative_domains` never links anything by itself.
