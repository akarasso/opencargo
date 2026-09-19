# Routing rules

A group resolves a name by asking its members in order. A routing rule says
which members are allowed to answer for which names — and nothing else. It is
the defence against dependency confusion: `@acme/*` is served by your hosted
repository or by nobody, so a package of that name on the public registry is
never installed in its place.

## What a rule can and cannot do

A rule only ever **removes** members from a resolution. There is no effect that
sends a name somewhere the group would not have looked, which is what makes
adding a rule safe: it can never open a path, only close one. It also never
grants a read permission it did not have.

```
name      acme-internal
format    npm
patterns  @acme/*
except    @acme/public-ui
effect    allow_members [npm-internal]      # or allow_hosted, or deny
```

`effect` is one of:

| effect | who may answer |
|---|---|
| `allow_members` | exactly the hosted repositories named, and no other |
| `allow_hosted` | **any** hosted repository of that format, present and future |
| `deny` | nobody |

`allow_hosted` is the convenient form and `allow_members` the safe one. Under
`allow_hosted`, a hosted repository added to the group six months from now
widens the rule without the rule changing, and anyone who may publish to — or
promote into — any hosted member of the group can make a package answer under
a prefix you presented as pinned. The screen says so where you choose it.

## Scope: the whole format, always

A rule applies to **every repository of its format**, present and future,
whether it is reached through a group or addressed by its own URL. There is no
way to narrow it, and a request that tries to carry one is refused rather than
served with the field ignored.

That is deliberate. A protection missing from a group created later is not a
protection, and a protection you can step around by changing the URL
(`/npm/proxy-npmjs/@acme/foo` instead of `/npm/group/@acme/foo`) is not one
either.

## Patterns, exceptions and spellings

`patterns` is a restricted glob: `*` stands for any run of characters,
separators included, and the match is anchored on the whole name. There is no
`?`, no alternation, no character class and no regular expression — a supply
chain rule has to be readable by someone who does not write regexes. The
language cannot express a literal `*`; name such a package in `except`.

Two names that the store serves as one row always match the same patterns.
Beyond that, a pattern is compared on a **coarsened** key: ASCII case folded
away everywhere, plus each format's own equivalences (`_`↔`-` for cargo, PEP
503 for PyPI, the `!x` escaping for Go). So `@ACME/foo` is filtered by
`@acme/*`, and a capital letter is not a way out.

`except` is compared on the **exact** store identity instead, never on the
coarsened key and never as a glob. An exception reopens precisely the spelling
you wrote: in npm, `except: @acme/public-ui` does not reopen
`@ACME/public-ui`; in Go, `github.com/acme/tool` does not reopen
`github.com/Acme/tool`. Coarsening a pattern refuses more, which is
conservative; coarsening an exception would reopen more, which is not.

The admin screen's **Test a name** box shows both keys beside the name you
typed, so a coarsening is visible rather than surprising.

## Several rules

There are no priorities and no ordering. Every rule whose patterns cover a
name applies, and the set of members allowed to answer is the **intersection**
of what they allow. Deny therefore wins by construction, two rules can never
contradict each other, and adding a rule can never open a path. A refusal is
imputable to *every* rule that refuses — deleting the one an error message
happened to name may well not lift it, which is why the API, the audit entry
and the screen all list the whole set.

## What a client sees

Nothing. A name that no allowed member has answers the 404 the group already
answered — no header, no body and no status code names a rule. `npm`, `pnpm`,
`cargo` and the rest fail the install on the missing internal package instead
of installing the public impostor, which is the point.

The cause is visible to an administrator: in `explain`, in the audit log and
in the refusal counters.

A rule never turns a member's outage into a failure and never hides one: a
refused member contributes nothing, and a `degraded` answer produced by
another member travels through unchanged.

One residual channel is worth knowing about. When the refused member is
exactly the one whose upstream is down, the request answers 404 at once where
it would otherwise have answered 502 after trying. What an observer can learn
from that is the *set of patterns* — never your internal inventory, because the
decision never looks at whether a name exists. The patterns are treated as
administration data, not as a secret, and every probe trips the first-sighting
alarm below.

## The cache

A member that a rule refuses receives no request, no cache lookup and no cache
entry. An answer cached before the rule was written simply becomes unreachable
— by the group and by the proxy's own URL — and the sweep reclaims the space in
its own time. Creating a rule does not delete anything; purge the repository if
you want the bytes gone now.

## Freshness, and what happens when a node falls behind

Each node keeps a compiled snapshot of the rules with a monotonic version, and
re-reads it every `routing.refresh_secs`. A write through the API refreshes the
writing node before it answers.

If a node cannot refresh for longer than `routing.max_snapshot_age_secs`, it
stops serving from what it knew: the **proxy** members of every format a rule
speaks for are refused. Hosted repositories are untouched, so the degradation
is confined to the surface the rules protect and it goes the closed way. The
alternative — serving the state from before a rule the node has not seen —
would mean the protection is absent exactly during the incident that motivated
it.

## Refusals in the audit log

A refusal happens at the cadence of a `pnpm install`, so it is recorded twice
over, at two different grains:

- the **first** refusal of a (name, repository addressed, member left out) in
  `routing.refusal_window_secs` is an audit entry carrying the caller —
  anonymous included — and every rule that refused;
- everything after it is the `opencargo_routing_refusals_total` counter.

The deduplication key holds no rule name, so renaming a rule or adding one that
sorts before it does not fire the same alarm again.

A refused name produces no policy resolution and no `would_block`: the routing
decides the path before the proxy resolves anything, so read the routing
section of the report rather than its `would_block` count.

## Trying a rule before activating it

```
POST /api/v1/routing-rules/explain
{ "repository": "npm-all", "name": "@ACME/widget" }
```

answers the members the resolver would consult, each `admitted` or
`refused_by: [...]`, with both keys and the snapshot version it decided on.
`explain` and the real resolution share the same decision on the same
snapshot; a dry run that diverged from it is how a wrong rule gets activated.

Pass a `candidate` rule in the same body to try one that is **not** stored:
reviewing a rule must not require publishing it.

## API

| Route | |
|---|---|
| `GET /api/v1/routing-rules` | the rules and the snapshot version |
| `POST /api/v1/routing-rules` | create |
| `GET/PUT/DELETE /api/v1/routing-rules/{name}` | read, replace, delete |
| `POST /api/v1/routing-rules/explain` | the dry run |

All admin-only. A deletion answers `still_refused_by`: the other rules that
still refuse the same names, so a reopening that did not happen is not reported
as one.

## Configuration

```toml
[routing]
refresh_secs = 30
max_snapshot_age_secs = 300
refusal_window_secs = 3600

[[routing.rules]]
name = "acme-internal"
format = "npm"
patterns = ["@acme/*"]
effect = "allow_members"
targets = ["npm-private"]
```

`[[routing.rules]]` is written **into an empty table, once**. Afterwards the
API owns the rules: one deleted through it does not come back at the next
restart, and a pattern hardened in the file is *not* applied. The startup log
names every rule the file declares that the stored set does not carry, so that
silence is never mistaken for a change that landed.

## What this does not protect

A rule binds clients that go through opencargo. A developer whose `.npmrc`
points straight at the public registry is not covered by it, and no registry
can cover them.

Publishing is not covered either: a rule is a read-path control, and refusing
a publish because a name is pinned elsewhere is a different feature.
