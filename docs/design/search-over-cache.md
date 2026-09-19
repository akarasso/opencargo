# Search over what the proxy has served

Design contract for: making a proxied package findable. Reads
`ports-and-adapters.md` (2.3, 2.6, 4.2) and the frozen amendment A1 v3 (C1, C6).
Architecture level only; columns and bounds belong to the code loop.

## 0. The gap

`SearchIndex` answers from `packages`, and `packages` holds hosted rows only. A
proxy member has no row there, so the two search surfaces disagree with the
registry they front:

- `GET /{repo}/-/v1/search` walks the group and its proxy members answer
  `NotFound` — `npm/leaves.rs`'s `SearchLeaf::proxy` is a stub with a comment
  saying so.
- `GET /api/v1/search` searches `packages` under a visibility predicate, so the
  UI says "no results" for a package the server has served ten minutes ago.

The failure is silent and it is the first thing a new user does. NuGet is the
one format that already answers search on a proxy member, by forwarding the
query to its upstream (`registry/nuget/upstream.rs`).

## 1. Decisions

**D1. The cache is indexed, upstream is not queried.** A search never leaves the
process. Forwarding `npm search` to an upstream would make a registry read
depend on an upstream's availability, its rate limit and its ranking, and would
answer with packages this server has never held; the group's existing
assertion — `search never asks the upstream` (`npm_proxy_test.rs`) — stays true.
What is indexed is what this server has actually served, which is what an
operator is asking about. NuGet keeps its upstream search: it is that protocol's
own surface, and it is additive to the index.

**D2. A package is indexed, its versions are not.** One row per
(repository, package name): the name, a description when the format's document
carries one, and the version last seen as that package's newest. Cached
*versions* are not rows.

- A proxy's version list is upstream state, not this server's inventory; the
  moment it is copied it is wrong, and nothing reads it. Version lookups already
  have an answer — the packument, the index lines, the project page — served
  from the cache and revalidated by TTL.
- The index would otherwise grow with every version of every package a `npm ci`
  touches, in a database whose single writer the proxy path shares.
- What the UI needs from a cached hit is "this name is here, through this
  repository"; the version is a hint, and a stale hint is honest as long as it
  is labelled `last seen`.

**D3. A sighting is written when the upstream is talked to, not on every read.**
The engine's warm path answers most requests without an exchange, and a write
per read would put an `UPDATE` on the hot path of `npm install`. So a fill and a
revalidation (`Stored`, `304 Not Modified`) index; a fresh cache hit does not.
The index therefore converges on a cache that was already warm before this
feature shipped, at most one write per package per TTL, and a package fetched
once is findable immediately — the fetch that returned 200 has already written
the row.

**D4. Indexing never fails a fetch.** The write is awaited (so a test that
fetched can search) but its failure is logged and dropped: the client gets its
package. The index is a read model, not a fact of the release chain.

**D5. Eviction does not unindex; purge and retire do.** The cache sweep evicting
an idle body does not make the package unavailable — the next request refetches
it — so the row stays, carrying `last_seen`. What removes rows is the operator
saying so: a cache purge of the repository, and `retire`, which takes them with
the repository row through the schema's cascade.

## 2. The port

One port, numbered **24** under A1 C1's table. This is an amendment proposal to
A1 at its next revision, not an inline edit of the contract; nothing else in
this design needs A1 to change.

```rust
// src/ports/search.rs, beside SearchIndex: same vocabulary, SearchScope and
// SearchQuery shared, so a caller scopes a cached search exactly as a hosted one.
#[async_trait] pub trait CachedPackageIndex: Send + Sync {
    /// Idempotent under (repository, name); `now` is the caller's clock.
    async fn remember(&self, seen: &Sighting<'_>, now: DateTime<Utc>) -> Result<(), StoreError>;
    /// `Some(q)`: relevance-ordered. `None`: a browse, the scope's rows in the
    /// adapter's natural order -- the same contract as `SearchIndex::search`.
    async fn search(&self, scope: SearchScope, q: Option<&SearchQuery>, limit: u32)
        -> Result<Vec<CachedPackage>, StoreError>;
    /// What a cache purge drops; returns how many rows went.
    async fn forget_repo(&self, repo: RepoId) -> Result<u64, StoreError>;
}
```

`Sighting` is the domain's write record (`{repository, format, name,
description, latest_version}`, borrowed in), `CachedPackage` the row read back.
Neither names a table, a document or a protocol: the format adapter is what
knows that npm's description is `packument["description"]` and that Cargo's
newest version is the last index line.

Why not a write side on `SearchIndex`: 2.3 refused one because the SQLite
adapter maintains `packages_fts` from its own `packages` writes and the method
would have had no real implementation. Here the opposite holds — nothing writes
these rows unless the proxy path says so — and the reader is a different read
model with a different lifecycle. Two ports, one vocabulary.

## 3. Who writes, who reads

- **Writes**: one use case, `app::search::seen`, called from each format's proxy
  metadata leaf with a `Sighting` that leaf built. The use case owns the rule
  (D3, D4); the leaf owns the document. `Cx` carries the port and the clock, so
  no leaf reaches for `AppState` (4.2's `cx.state` row stays 0).
- **Reads**: `app::search::find` merges the two indexes for `/api/v1/search` —
  hosted rows first, then cached rows whose name no hosted row already
  answered — and `SearchLeaf::proxy` answers a member's npm search from the
  cached index scoped to that member. Merging is the use case's rule, not the
  HTTP adapter's.
- **Removes**: the cache purge use case calls `forget_repo`; `retire` needs no
  code, the rows hang off the repository row.

## 4. Invariants

1. A package fetched through a proxy repository is findable in that repository
   and in every group that lists it, by name and by a word of its description
   where the format carries one.
2. A hosted row always wins a name a cached row also carries: one result, marked
   hosted, never two.
3. A cached row is visible exactly to the callers its repository is visible to:
   the `PublicOnly` scope is the same predicate as the hosted one.
4. No search leaves the process (NuGet's own upstream search excepted).
5. A failed index write never changes what a client is served.
6. Nothing in `crates/domain` names a document, a table or a protocol: the
   domain holds `Sighting`, `CachedPackage` and nothing more.

## 5. Test matrix

| # | proof | where |
|---|---|---|
| 1 | fetch a packument through a proxy, then `/api/v1/search` finds it, marked proxied, naming the repository | `tests/search_cache_test.rs` |
| 2 | the same package found by `npm search` on the proxy member and on a group above it, with no upstream hit | same |
| 3 | a hosted package of the same name collapses the cached one, marked hosted | same |
| 4 | a private proxy repository's cached rows are invisible to an anonymous search and visible to an admin | same |
| 5 | a second fetch of the same package leaves one row, with the newer `last_seen` | same |
| 6 | a cache purge drops the rows; the package is findable again after a refetch | same |
| 7 | Cargo, PyPI and Go sightings: an index/page/list fetch indexes the name | same |
| 8 | the index write failing leaves the fetch a 200 | adapter/use-case unit test |
| 9 | the adapter and the fake agree on `remember`/`search`/`forget_repo` | `tests/common/contract.rs` |

## 6. Not in this design

- Ranking across the two indexes. Hosted first, cached after, each in its own
  index's order; a single relevance ordering over two tables is a query the
  SQLite adapter cannot write without a UNION whose `rank` is meaningless.
- A background crawler that populates the index from an upstream's catalog.
  Nexus does this; it is a different feature, with a different failure mode, and
  D1 is what this one is worth.
- OCI: `search` has no place in the distribution protocol and the UI lists
  images from the hosted rows; a cached image is reachable by its reference,
  which is what a client has when it pulls.
