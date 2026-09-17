# Proxy and group repositories for every format

Design contract for: proxy/group repositories for Cargo, Go and OCI at parity with npm; nested OCI image names; OSV
severity done right; one generic resolver; per-format upstream strategies; a proper proxy cache; tests as proof; an
incremental delivery plan. Line references are to commit `ba21753`; re-grep before editing.

## 1. Decisions

1. Three closed sets become enums round-tripped from the DB: `RepoKind`, `Format`, `CachePolicy`; a corrupt column value
   is `Internal`, never a silent default.
2. One generic resolver (`registry::resolve`) replaces the three npm group loops (`src/registry/npm/mod.rs:511-589,
   739-822, 855-905`): members, depth, cycles, per-member `ensure_can_read`, not-found-vs-failure policy.
3. Two newtype identities: `UrlRepo`, the repository the client addressed, is the only name in `Location`,
   `dist.tarball`, cargo `dl`/`api`, `tags/list.name`; `CacheRepo`, the member owning bytes, is the only name in cache
   keys, storage paths and `repository_id`. Mixing them does not compile.
4. Per-format behaviour is a sync, generic `UpstreamStrategy` with an associated `Artifact`; the handler passes it to
   `ProxyEngine::fetch::<S>`. No `Format -> dyn Strategy` registry: the proxy layer never names a format.
5. Cached artifacts live only in `proxy_cache_entries` (migration 013, `ON DELETE CASCADE`) and under
   `_proxy_cache/{member}/...`; proxied OCI blobs/manifests never enter `oci_*` tables, so the refcount guard
   (`oci/mod.rs:207-218`) and manifest GC (`:930-966`) stay hosted-only.
6. Upstream failures are `AppError::BadGateway` (502), never 404; negative entries only for what the strategy's
   `classify_status` calls a miss (404/410; OCI adds the post-token 401/403).
7. Upstream credentials and `dl_allow_private` live in config and environment, loaded into `AppState`; never in SQLite
   or the API. Upstream-chosen URLs (cargo `dl`, redirect hops) are held to `is_blocked_ip`.
8. The OSV gate runs before the first write; a blocked publish leaves no file and no row. An OSV outage under
   `block_on_critical` follows `vuln_scan.fail_closed` (default false, logged).
9. Every cache file goes to `{path}.part-{uuid}` behind an RAII `PartFile` (one open `tokio::fs::File`) whose `Drop`
   unlinks synchronously unless `commit` renamed it into place, so a refresh never truncates a reader's inode; the cap
   counts bytes read; a sweep reclaims abandoned parts. Hosted and proxied bytes are served through one cache-neutral `Payload`; a HEAD hit is answered from the cache row.
10. New crate `cvss` 2.2 (Apache-2.0 OR MIT, MSRV 1.85); `tokio-util` (`io`) and `semver` become direct deps.

## 2. Module layout

```
src/
  error.rs / main.rs          + AppError::BadGateway(String) -> 502; --osv-base-url / OPENCARGO_OSV_BASE_URL next to --base-url (:20-22)
  config.rs                   VulnScanConfig{osv_base_url, fail_closed, max_concurrency} + manual Default; RepositoryConfig.{upstream_auth, token_realms, dl_allow_private} + derive(Default); CleanupConfig::default
  auth/middleware.rs          + is_cargo_config: GET|HEAD /{repo}/index/config.json passes the anonymous gate (section 4)
  db/{kinds,proxy_cache}.rs NEW RepoKind, Format, RepoSpec, validate_spec, check_repository_names, Repository::{kind,fmt,members}; CacheEntry, NewEntry, entry fns
  db/mod.rs                   migrate() += migrations/013_proxy_cache_entries.sql (NEW) with `?`; init_repositories uses validate_spec
  proxy/mod.rs                validate_upstream_url (hardened), is_blocked_ip, re-exports
  proxy/strategy.rs    NEW    UpstreamStrategy, CachePolicy, Ttl, Transfer, CacheKey
  proxy/engine.rs      NEW    ProxyEngine, Cached, Payload, Src, PartFile, cache_path; singleflight.rs = per-key lock with wait deadline
  proxy/auth.rs        NEW    UpstreamAuth, UpstreamCreds, BearerChallenge, TokenCache, acquire_token
  proxy/purge.rs       NEW    purge_repository(state, repo): proxy rows+files, group fan-out
  registry/resolve.rs  NEW    Cx, UrlRepo, CacheRepo, Outcome, Leaf, first_hit, collect
  registry/publish.rs  NEW    publish_gate, PreScan, finalize_publish (moved from mod.rs:298-363)
  registry/npm/{mod,routes,read,publish,leaves,upstream,packument}.rs  cargo/{mod,routes,index,download,publish,leaves,upstream}.rs
  registry/go/{mod,routes,read,publish,escape,leaves,upstream}.rs  oci/{mod,routes,routing,paths,refs,blobs,manifests,tags,uploads,leaves,upstream}.rs
  server.rs                   build_router merges npm/cargo/go/oci ::routes(); AppState.{proxy,upstream_auth}; build_state scans the per-repo env overrides (section 7)
  server/rewrite.rs    NEW    pre_route = oci::routing::rewrite_nested_name . go::routes::rewrite_module_path
  storage/{mod,filesystem}.rs + rename, read_stream, remove_stale_parts; FilesystemStorage::resolve
  telemetry/vulns/mod.rs      replaces telemetry/vulns.rs in commit 4 (signatures of section 9); osv/severity/deps.rs = Agent OSV
  telemetry/cleanup.rs        run_cleanup + sweep_proxy_cache (pub(crate), unit-tested); the proxy sweep ignores cleanup.enabled
  api/repositories.rs         validate_spec on create/update; purge for proxy|group; delete cascade
  api/vulns.rs                Format::osv_ecosystem(); assess + persist
tests/common/{mod,fake_osv,upstream_tap}.rs fake_upstream/{mod,npm,cargo,go,oci}.rs; tests/{npm,cargo,go,oci}_proxy_test.rs cargo_test.rs oci_nested_test.rs group_resolver_test.rs vuln_test.rs {cargo,go}_e2e_test.rs docker_cli_e2e_test.rs
```

Per-format roles: `routes.rs` = `pub fn routes() -> Router<AppState>` (from `src/server.rs:195-322`); `read.rs`/`index.rs`/
`blobs.rs`... = thin handlers; `leaves.rs` = `Leaf` impls; `upstream.rs` = strategy + `Artifact` enum; `publish.rs`/
`uploads.rs` = hosted-only writes. No file above ~400 lines, no function above ~80.

## 3. Core types

```rust
// src/db/kinds.rs — both enums: #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)] #[serde(rename_all = "lowercase")]
pub enum RepoKind { #[default] Hosted, Proxy, Group } // Default only so RepositoryConfig can derive(Default) (commit 3)
pub enum Format { #[default] Npm, Cargo, Oci, Go, Pypi }
impl RepoKind { pub const fn as_str(self) -> &'static str; }
impl Format { pub const fn as_str(self) -> &'static str; pub const fn osv_ecosystem(self) -> Option<&'static str>;
    pub const fn supports_kind(self, kind: RepoKind) -> bool; }
impl crate::db::Repository { pub fn kind(&self) -> AppResult<RepoKind>; pub fn fmt(&self) -> AppResult<Format>;
    pub fn members(&self) -> Vec<String>; }
pub struct RepoSpec<'a> { pub name: &'a str, pub kind: RepoKind, pub format: Format,
    pub upstream: Option<&'a str>, pub members: &'a [String] }
pub async fn validate_spec(pool: &SqlitePool, spec: &RepoSpec<'_>, pending: &[(&str, Format)]) -> AppResult<()>;
```

`osv_ecosystem`: Npm "npm", Cargo "crates.io", Go "Go", Oci/Pypi None; replaces the literals at `npm/mod.rs:333`, `cargo/mod.rs:361`, `go/mod.rs:369`
and the match at `src/api/vulns.rs:190-195`. `supports_kind`: Pypi -> Hosted only. `validate_spec`: `name` matches `[a-z0-9][a-z0-9._-]{0,63}`
and contains no `..` (today `create_repository`, `src/api/repositories.rs:46-127`, applies no rule to `body.name`; section 5 makes the name a raw
storage segment and the purge prefix, and `safe_path`, `src/storage/filesystem.rs:29`, 400s any path containing `..`; every in-tree name,
`npm-all`, `oci-hosted`, README's `npm-private`, passes); Proxy needs `upstream` passing `validate_upstream_url` and `supports_kind(Proxy)`; Group
needs at least one member, each existing (DB or `pending`, the config list being seeded), same format, no self-reference, no cycle; Hosted takes
neither. The name rule runs on create, seed and update (`update_repository` cannot rename, so it re-checks the stored name). The API whitelist (`src/api/repositories.rs:69`) keeps rejecting `pypi`; the seed
keeps accepting hosted pypi. `config.rs` re-exports `RepositoryType`/`RepositoryFormat` as aliases so tests keep compiling.

```rust
// src/registry/resolve.rs
pub const MAX_GROUP_DEPTH: u32 = 5;
#[derive(Clone, Copy, Debug)] pub struct UrlRepo<'a>(pub &'a str); // same derives: CacheRepo<'a>(pub &'a Repository)
pub struct Cx<'a> { pub state: &'a AppState, pub auth: Option<&'a AuthUser>, pub url: UrlRepo<'a>, pub failure: FailurePolicy }
#[derive(Clone, Copy, PartialEq, Eq, Debug)] pub enum FailurePolicy { NotFound, BadGateway } // commit 4 only; 4b deletes field and enum
pub enum Outcome<T> { Found(T), NotFound }
pub struct Upstream { pub base: reqwest::Url, pub auth: Option<UpstreamAuth>, pub token_realms: Vec<reqwest::Url>, pub dl_allow_private: bool } // ::for_member

#[async_trait::async_trait]
pub trait Leaf: Send + Sync {
    type Out: Send;
    async fn hosted(&self, cx: &Cx<'_>, member: CacheRepo<'_>) -> AppResult<Outcome<Self::Out>>;
    async fn proxy(&self, cx: &Cx<'_>, member: CacheRepo<'_>, up: &Upstream) -> AppResult<Outcome<Self::Out>>;
}
pub struct Collected<T> { pub hits: Vec<T>, pub degraded: Option<String> }
pub async fn first_hit<L: Leaf>(cx: &Cx<'_>, repo: &Repository, leaf: &L) -> AppResult<L::Out>; // collect: same args -> Collected<L::Out>
```

`Leaf` is used through generics only, so it need not be object safe; `async_trait` (already a dependency) gives `Send` futures. A private `walk(cx,
repo, leaf, depth, seen, sink)` is the one boxed recursion, as `get_package_group` is today. `Upstream::for_member` reads `member.upstream_url`
(`Internal` when absent, as `npm/mod.rs:465-469`) and `state.upstream_auth[member.name]` (section 7). `Cx.failure` is read where section 10 spells
`BadGateway` (`first_hit`: no hit + failure; `collect`: zero hits + failure): under `FailurePolicy::NotFound` both answer `NotFound` (today's blanket
404; `degraded`/`Warning: 199` need a hit, unaffected). Commit 4 builds every `Cx` with `NotFound`; 4b deletes field and enum, both sites become
unconditional `BadGateway`.

```rust
// src/proxy/strategy.rs
pub const DEFAULT_MAX_UPSTREAM_BYTES: u64 = 100 * 1024 * 1024;
pub enum Ttl { Default, Secs(u64) }  pub enum CachePolicy { Immutable, Ttl(Ttl) }  pub enum Transfer { Buffered, Streamed }
pub struct CacheKey { pub kind: &'static str, pub key: String }
pub trait UpstreamStrategy: Send + Sync {
    type Artifact: std::fmt::Debug + Send + Sync; // no format() hook: the proxy layer never names a format (decision 4)
    fn upstream_url(&self, up: &Upstream, a: &Self::Artifact) -> AppResult<reqwest::Url>;
    fn cache_key(&self, a: &Self::Artifact) -> CacheKey;
    fn store_key(&self, a: &Self::Artifact, _body_sha256: &str) -> CacheKey { self.cache_key(a) }
    fn verify_headers(&self, _a: &Self::Artifact, _h: &HeaderMap, _body_sha256: &str) -> AppResult<()> { Ok(()) }
    fn cache_policy(&self, a: &Self::Artifact) -> CachePolicy;
    fn transfer(&self, _a: &Self::Artifact) -> Transfer { Transfer::Buffered }
    fn max_bytes(&self, _a: &Self::Artifact) -> u64 { DEFAULT_MAX_UPSTREAM_BYTES }
    fn request_headers(&self, _a: &Self::Artifact) -> Vec<(HeaderName, HeaderValue)> { Vec::new() }
    fn expected_sha256(&self, _a: &Self::Artifact) -> Option<String> { None }
    fn bearer_scope(&self, _a: &Self::Artifact) -> Option<String> { None }
    fn head_via_get(&self, _a: &Self::Artifact) -> bool { true }
    fn classify_status(&self, _a: &Self::Artifact, s: StatusCode) -> Classified { // 404 | 410 -> Miss, else Fail
        if matches!(s.as_u16(), 404 | 410) { Classified::Miss } else { Classified::Fail } }
}
#[derive(Clone, Copy, PartialEq, Eq, Debug)] pub enum Classified { Miss, Fail }

// src/db/proxy_cache.rs
#[derive(Debug, Clone, sqlx::FromRow)] pub struct CacheEntry { pub id: i64, pub repository_id: i64, pub kind: String, pub cache_key: String,
    pub status: i64, pub storage_path: Option<String>, pub content_type: Option<String>, pub etag: Option<String>,
    pub digest: Option<String>, pub size: i64, pub fetched_at: String, pub expires_at: Option<String>, pub last_used_at: String }
pub struct NewEntry<'a> { .. } // the CacheEntry columns borrowed, minus id/timestamps, plus ttl_secs: Option<u64>
// all `pub async fn (pool: &SqlitePool, ..) -> Result<_, sqlx::Error>`:
// get_entry(repository_id, kind, key) -> Option<(CacheEntry, fresh: bool)>; upsert_entry(&NewEntry) -> ();
// touch_entry(id, extend_ttl_secs: Option<u64>) -> (); delete_entries(repository_id) -> u64;
// delete_legacy_meta(repository_id) -> u64; evictable_entries(idle_days) -> Vec<CacheEntry>

// src/proxy/engine.rs
#[derive(Clone, Copy)] pub struct TtlConfig { pub default_secs: u64, pub negative_secs: u64 }
#[derive(Clone, Copy)] pub struct Timeouts { pub connect: Duration, pub read_idle: Duration, pub buffered_total: Duration, pub singleflight_wait: Duration }
impl Timeouts { pub fn from_connect_secs(n: u64) -> Self; } // connect n, read_idle 3n, buffered_total 6n, singleflight_wait 6n
pub struct Cached { pub entry: CacheEntry, pub stale: bool }  // impl Cached { pub fn into_payload(self) -> Payload; }
impl Outcome<Cached> { pub fn into_payload(self) -> Outcome<Payload>; } // Found(c) -> Found(c.into_payload()), NotFound -> NotFound
pub enum Src { File(String), Bytes(Bytes), HeadOnly }
pub struct Payload { pub src: Src, pub size: u64, pub content_type: Option<String>, pub digest: Option<String>, pub stale: bool }
impl Payload { pub fn file(storage_path: String, size: u64) -> Self; pub fn bytes(b: Bytes) -> Self;
    pub fn head_only(size: u64, content_type: Option<String>, digest: Option<String>) -> Self; }
pub struct PartFile { rel: String, resolved: PathBuf, file: Option<tokio::fs::File>, committed: bool } // Drop: std::fs::remove_file(&resolved) unless committed
impl PartFile { pub async fn new(storage: &FilesystemStorage, rel: String) -> AppResult<Self>; // resolve + create_dir_all + File::create, once
    pub async fn write_chunk(&mut self, chunk: &[u8]) -> AppResult<()>; // write_all on the open handle
    pub async fn commit(mut self, storage: &FilesystemStorage, final_rel: &str) -> AppResult<()>; } // flush, sync_data, drop handle, rename, committed = true
#[derive(Clone)] pub struct ProxyEngine { http: reqwest::Client, storage: Arc<FilesystemStorage>,
    db: SqlitePool, tokens: Arc<TokenCache>, inflight: Arc<Singleflight>, ttl: TtlConfig }
impl ProxyEngine {
    pub fn new(storage: Arc<FilesystemStorage>, db: SqlitePool, timeouts: Timeouts, ttl: TtlConfig) -> Self;
    pub async fn fetch<S: UpstreamStrategy>(&self, s: &S, up: &Upstream, member: CacheRepo<'_>, a: &S::Artifact) -> AppResult<Outcome<Cached>>;
    pub async fn head<S: UpstreamStrategy>(&self, s: &S, up: &Upstream, member: CacheRepo<'_>, a: &S::Artifact) -> AppResult<Outcome<Payload>>;
    pub async fn bytes(&self, c: &Cached) -> AppResult<Bytes>;
    pub async fn stream_response(&self, p: &Payload, extra: Vec<(HeaderName, HeaderValue)>) -> AppResult<Response>;
    pub async fn purge_repo(&self, member: CacheRepo<'_>) -> AppResult<()>;
    async fn resolve_row<S: UpstreamStrategy>(&self, s: &S, a: &S::Artifact, row: CacheEntry) -> Option<CacheEntry>; // private; section 5 step 3
}
pub fn cache_path(member: CacheRepo<'_>, key: &CacheKey) -> String; // infallible: check_repository_names refused startup otherwise (section 5)
// src/proxy/singleflight.rs
impl Singleflight { pub async fn acquire(&self, key: &str, wait: Duration) -> Option<Guard>; } // None = waited past `wait`

// src/proxy/auth.rs
#[derive(Clone, Debug, serde::Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum UpstreamAuth { Basic { username: String, password: String }, Bearer { token: String } }
#[derive(Clone, Debug, Default)] pub struct UpstreamCreds { pub auth: Option<UpstreamAuth>, pub token_realms: Vec<reqwest::Url>, pub dl_allow_private: bool }
pub struct BearerChallenge { pub realm: reqwest::Url, pub service: Option<String>, pub scope: Option<String> }
pub fn parse_bearer_challenge(www_authenticate: &str) -> Option<BearerChallenge>;
pub struct TokenCache { inner: Mutex<HashMap<(String, String), (String, Instant)>> } // (realm, scope) -> token
pub(crate) async fn acquire_token(http: &Client, cache: &TokenCache, ch: &BearerChallenge, up: &Upstream) -> AppResult<String>;

// src/storage/mod.rs additions
async fn rename(&self, from: &str, to: &str) -> Result<(), AppError>;
async fn read_stream(&self, path: &str) -> Result<(u64, Pin<Box<dyn tokio::io::AsyncRead + Send>>), AppError>;
async fn remove_stale_parts(&self, prefix: &str, older_than: Duration) -> Result<u64, AppError>; // `*.part-*` by mtime
// src/storage/filesystem.rs: the private safe_path (:28) gains a public wrapper, used once, by PartFile::new
impl FilesystemStorage { pub fn resolve(&self, path: &str) -> Result<PathBuf, AppError>; }

// src/registry/publish.rs
pub struct PreScan(Option<ScanResult>);
pub async fn publish_gate(state: &AppState, format: Format, metadata_json: &str) -> AppResult<PreScan>;
#[allow(clippy::too_many_arguments)] pub async fn finalize_publish(state: &AppState, format: Format, repo_name: &str,
    package_name: &str, version_str: &str, version_id: Option<i64>, metadata_json: &str, published_by: &str, pre: PreScan) -> AppResult<()>;
```

`RepoKind`/`Format` also implement `FromStr` (`Err = AppError`). `AppState` gains `proxy: ProxyEngine` (replacing `proxy_client` and
`proxy_default_ttl_secs`) and `upstream_auth: Arc<HashMap<String, UpstreamCreds>>` (`proxy/auth.rs`, keyed by repository name; a missing key is
`UpstreamCreds::default()`), whose three fields `for_member` copies into `Upstream` beside `base`. `head_via_get`: how a HEAD *miss* is answered, a full cached GET (manifests, default) or an upstream HEAD without download
(OCI blobs); a hit never consults it. `classify_status`: which upstream status is an authoritative miss (negative entry) rather than a failure;
default 404/410, widened by `OciUpstream` (section 7). `store_key` differing from `cache_key(a)` (OCI `Tag`) stores the body Immutable under
`store_key` and makes `cache_key(a)` a pointer row (`storage_path NULL`, `digest` = body sha256) under `cache_policy(a)`; `verify_headers` runs before
any write. Every other hook is a pure function of the artifact.

`Payload` is the one serving type and is cache-neutral: `Cached::into_payload()` maps a row to `Src::File(storage_path)` +
`size`/`content_type`/`digest`/`stale`; `Outcome<Cached>::into_payload` lifts it over `Found`, so `fetch(..)?.into_payload()` is an
`Outcome<Payload>`; a hosted arm builds `Payload::file(path, size)` from its own row (`oci_blobs`, `versions.tarball_path`) with no `CacheEntry`; a
handler that rewrote JSON returns `Payload::bytes`. `stream_response` serves all three `Src`s (`HeadOnly` = headers and `Content-Length`, empty body).
`PartFile::new(storage, rel)` calls `storage.resolve(&rel)` once, creates the parent and opens the file once; `write_chunk` is `write_all` on that
handle, never `StorageBackend::append` (`src/storage/filesystem.rs:122-136` re-runs `safe_path`, which `canonicalize`s an existing file, then opens,
flushes and stats per call: ~10^5 round-trips over a 4 GiB layer; it stays for OCI chunked uploads, `oci/mod.rs:359`). `Drop` is `let _ =
std::fs::remove_file(..)` unless `committed`, synchronous because `Drop` cannot await and a spawned task would leak at shutdown; `commit` flushes,
`sync_data`s, drops the handle, awaits `rename`.

Timeouts: the client is built with `.connect_timeout(connect)` and `.read_timeout(read_idle)` (per-chunk idle; reqwest 0.12.28), never with the
client-wide `.timeout` of `ProxyClient::new` (`src/proxy/mod.rs:72-73`: 30 s over the whole body, fatal to a multi-minute `oci-blob`). Only
`Transfer::Buffered` gets a total bound (`buffered_total` around send + read); `from_connect_secs` = 10 / 30 / 60 / 60 s by default
(`src/config.rs:147-155`); a streamed transfer is bounded by idleness and `max_bytes`, never by wall time.

Lock ownership: `Singleflight::acquire(key, timeouts.singleflight_wait)` has one call site, the first line of `fetch`; its `Guard` drops when `fetch`
returns (`Cached` never carries it), so chained fetches on one path (cargo `Config` -> `Index` -> `Crate`) never hold two. The lock is bounded, the
transfer is not: a waiter past `singleflight_wait` (60 s) proceeds unlocked and re-reads the row, so it either serves the leader's finished entry or
duplicates the download (own part file, own `upsert_entry`, identical bytes, last writer wins). `head` takes no lock and reads the cache first,
applying section 5 steps 2 and 3 in order: a fresh `status <> 200` row is `Outcome::NotFound` (a tag that 404'd under GET must not HEAD as `200`
with `Content-Length: 0`; `head_on_negative_row_is_404`), a `status 200` row goes through `resolve_row`, and a hit is `touch_entry`'d and answered
as `Payload::head_only(size, content_type, digest)` from the row, so a warm cache answers docker's per-layer HEADs with zero upstream traffic and keeps pulling while the upstream is down. Only
a miss consults `head_via_get`: `true` -> `fetch(..)?.into_payload()` (the GET warms the cache); `false` -> `forward_head`, one upstream HEAD through
`send_with_auth` that touches neither `inflight` nor the DB and maps 200 -> `Found(Payload::head_only(..))` from the headers, `classify_status ==
Miss` -> `NotFound`, else `BadGateway`. HEAD-then-GET on a manifest is one upstream GET.

## 4. Request flows

Common handler shape (replaces the raw-string `match repo.repo_type` at `npm/mod.rs:382-393, 640-651, 854-917` and the missing dispatch in
cargo/go/oci): validate path params (`src/registry/mod.rs:176-285`) before any URL or key is built; `load_repo`; `ensure_can_read(requested)`
(`:37-60`, unchanged); build `Cx { state, auth, url: UrlRepo(&repo.name) }` (plus `failure` until 4b) and a leaf; `first_hit` or `collect`; respond
with `cx.url`. The resolver dispatches on `repo.kind()?`: Hosted -> `leaf.hosted`; Proxy -> `leaf.proxy` with `Upstream::for_member`; Group ->
`parse_group_members` (`src/db/mod.rs:457-467`) in order, per member: load, `ensure_can_read` (Unauthorized/Forbidden -> silent skip), same `fmt()`
else warn+skip, `seen.insert(id)` else skip, nested group -> `depth + 1`. Single artifacts use `first_hit`; enumerations (cargo index, go
`@v/list`/`@latest`, OCI `tags/list`, npm search) use `collect` and merge in their own module. Binary leaves (`TarballLeaf`, `CrateLeaf`, go `.zip`,
`BlobLeaf`, `ManifestLeaf`) have `Out = Payload`: the hosted arm returns `Payload::file(path, size)` from its own table, the proxy arm
`fetch(..)?.into_payload()`, and the handler calls `stream_response` once; JSON leaves go `fetch` -> `bytes()` -> rewrite with `cx.url`. Writes
(publish, yank, dist-tags PUT/DELETE, OCI uploads, manifest PUT, blob/manifest DELETE) call `ensure_hosted` + `ensure_format`; dist-tags writes
(`npm/mod.rs:1045-1126`) and OCI deletes (`oci/mod.rs:203, 877`) gain the check.

npm (reads migrated in foundation commit 4 with today's status codes; the search and dist-tags changes are Agent NPM's):
- `GET /{repo}/{name}` and `/{repo}/@{scope}/{name}`: `PackumentLeaf { name, abbreviated }`; hosted arm =
  `npm/mod.rs:397-459` moved to `packument.rs`; proxy arm = `NpmArtifact::Metadata { name }`; the handler applies
  `rewrite_tarball_urls(&mut json, base_url, cx.url, name)` (`src/proxy/mod.rs:303-328`), then abbreviated stripping.
- `GET .../-/{filename}`: `TarballLeaf { name, filename }`; hosted arm = version by `tarball_path.ends_with(filename)`
  (`:669`) + `record_download` -> `Payload::file(tarball_path, size)`; proxy arm = `NpmArtifact::Tarball`, Immutable,
  Streamed.
- `GET /{repo}/-/v1/search`: `collect` over `SearchLeaf` (hosted arm = local `packages` rows; proxy arm =
  `Ok(NotFound)`), dedup, re-paginate; nested groups now recurse. `GET .../-/package/{name}/dist-tags`: `DistTagsLeaf`;
  hosted from `dist_tags` rows, proxy from the cached packument (the handler, `:1012-1043`, reads `packages` only, so a scoped
  name 404s through proxy/group). Two distinct bugs: `src/server.rs:222-230` registers only the scoped pair, so the unscoped
  `/{repo}/-/package/{name}/dist-tags[/{tag}]` matches no route and `npm dist-tag ls|add|rm <pkg>` 404s on hosted repos too.
  Foundation commit 1 registers the unscoped pair in `npm/routes.rs` (the handlers already read a missing `scope`,
  `extract_package_name`, `src/registry/mod.rs:19-24`; verified against axum 0.8.8/matchit 0.8.4 that the four routes coexist and
  `@{scope}` never captures an unscoped name), pinned by `npm_test::unscoped_dist_tags_get_put_delete` on a hosted repo before
  Agent NPM's leaf lands.

Cargo:
- `GET /{repo}/index/config.json`: never proxied; `{ "dl": "{base}/{url}/api/v1/crates", "api": "{base}/{url}" }` with
  `url = cx.url` (`cargo/mod.rs:93-96` shape), plus `"auth-required": true` when the repo is private. Stays readable
  anonymously: cargo fetches `config.json` before it knows whether to send a token and learns to from `auth-required`.
  Two gates block that, both opened in commit 1: `auth_middleware` 401s every tokenless GET when `anonymous_read =
  false` (`src/auth/middleware.rs:166, 183`, before any handler, no `Www-Authenticate` for cargo to retry on), so it
  gains `is_cargo_config` = `GET|HEAD /{repo}/index/config.json` beside `is_oci` (`:61`), passing the tokenless request
  through (a sent token is still validated); and `config_json`, which today checks existence only (`:84-99`), gains
  `ensure_format(Cargo)` + `ensure_can_read` when a token was sent (no read -> 403), disclosing only existence and
  format. The two land together, the handler side raw (`Option<Extension<AuthUser>>` as `:191`) until Agent CARGO
  moves it onto `Cx`: the middleware alone would 200 every repo of any format or visibility. Index and download enforce
  `ensure_can_read` unconditionally; `cargo_e2e_test.rs` spawns with `anonymous_read = false`, proving the bootstrap.
- `GET /{repo}/index/{prefix..}/{name}`: `validate_package_name("cargo")`; `collect` over `IndexLeaf { name }`; hosted
  arm = `build_index_line` (`:44-78`); proxy arm = `CargoArtifact::Index { name }` split into lines. `merge_index_lines`
  dedups by `vers`, first member wins, joins with `\n`; empty -> 404. `ETag` = sha256 of the body, `If-None-Match` -> 304.
- `GET /{repo}/api/v1/crates/{name}/{version}/download`: validate both; `first_hit` over `CrateLeaf`; hosted arm =
  `:406-427`; proxy arm = `fetch(Config)` for the upstream `dl` template, `fetch(Index { name })` for the `cksum` of
  `version` (absent -> NotFound), then `fetch(Crate { name, version, dl, cksum })` Immutable, Streamed, sha256 verified.
  `Content-Type: application/x-tar`.
- yank/unyank/publish: `ensure_hosted` replaces `cargo/mod.rs:461-466, 512-517`.

Go (`rewrite_module_path` and `version_dispatch` move from `src/server.rs:546-591, 629-655` to `go/routes.rs`):
- Handlers receive the module GOPROXY-escaped (`github.com/!burnt!sushi/toml`); `validate_escaped_module` guards every
  read; the hosted arm `unescape`s before `get_package` (rows keep the raw path, `go/mod.rs:308-324`); the proxy arm
  keeps the escaped form for URL and cache key. Publish keeps the strict validator (`src/registry/mod.rs:218-227`).
- `@v/list`: `collect` over `ListLeaf`; union, dedup, one per line. A member is `Found(versions)` only when it knows the
  module (hosted: a `packages` row, even empty; proxy: upstream 200, even empty); zero hits -> 404 (today
  `go/mod.rs:36-39` answers 200 empty, which under `GOPROXY=a,b,direct` ends resolution instead of moving on).
- `@latest`: `collect` over `LatestLeaf`; max by `semver` (strip `v`; pseudo-versions parse as pre-releases), lexical
  fallback. Hosted `@latest` and `.info` render `Time` as RFC 3339 (today `YYYY-MM-DD HH:MM:SS`, `go/mod.rs:91, 134`,
  which the go tool rejects).
- `.info/.mod/.zip`: `version_dispatch` does one `ensure_can_read`, then `first_hit` over `FileLeaf { kind, version }`;
  proxy arm = `GoArtifact::{Info, Mod, Zip}`, Immutable when `is_canonical_version`, else `Ttl::Secs(600)`; `.zip`
  Streamed; bodies byte-exact (h1 hashes).

OCI (after the nested-name rewrite, `name` may be `team/app`):
- Every route parses `OciRef { repo, name }` and calls `validate_package_name("oci", &name)` (today only
  `oci/mod.rs:283, 714` validate).
- `GET|HEAD blobs/{digest}`: `first_hit` over `BlobLeaf { digest, head: bool }`; hosted arm = `oci_blobs` row +
  `paths::blob_path` -> `Payload::file(..)` (or `head_only` from the row's size); proxy arm = `OciArtifact::Blob`,
  Immutable, Streamed, `head_via_get = false`, `engine.head` or `engine.fetch` per `head`. `HeadOnly` answers status,
  `Content-Length` and `Docker-Content-Digest` with no body.
- `GET|HEAD manifests/{reference}`: `first_hit` over `ManifestLeaf { reference, head }`; hosted arm = `:541-593` ->
  `Payload::file`; proxy arm = digest -> `Manifest { name, reference }` Immutable with `expected_sha256`; tag ->
  `Tag { name, tag }` (section 7). `Docker-Content-Digest` is content-derived.
- `GET tags/list`: `collect` over `TagsLeaf`; union, sort, `n`/`last` applied locally (`:1013-1030`); `"name":
  "{cx.url}/{name}"`. Push and delete routes: `ensure_hosted` + `ensure_format(Oci)`; `Location` keeps
  `/v2/{cx.url}/{name}/...` (`:294, 361, 509, 840`).

## 5. Cache design and migration 013

| format | kind | cache_key | policy | transfer |
|---|---|---|---|---|
| npm | `npm-metadata` | `{name}` | Ttl(Default) | Buffered |
| npm | `npm-tarball` | `{name}/{filename}` | Immutable | Streamed |
| cargo | `cargo-config` | `config.json` | Ttl(Default) | Buffered |
| cargo | `cargo-index` | `{name lowercase}` | Ttl(600) + ETag | Buffered |
| cargo | `cargo-crate` | `{name}/{version}` | Immutable, sha256 = cksum | Streamed |
| go | `go-list`, `go-latest` | `{escaped module}` | Ttl(600) | Buffered |
| go | `go-info`, `go-mod`, `go-zip` | `{escaped module}/{version}` | Immutable if canonical else Ttl(600) | zip Streamed |
| oci | `oci-manifest` | `sha256/{hex}` | Immutable, sha256 = digest | Buffered (10 MiB) |
| oci | `oci-tag` | `{name}/{tag}` | Ttl(Default), pointer row | Buffered, body under `store_key` |
| oci | `oci-blob` | `sha256/{hex}` | Immutable, sha256 = digest | Streamed (4 GiB) |
| oci | `oci-tags` | `{name}` | Ttl(600) | Buffered |

Keys are `(repository_id, kind, cache_key)`, `repository_id` always the member. Storage path: `cache_path(member, key) =
"_proxy_cache/{member.name}/{kind}/{h[..2]}/{h}"`, `h = hex(sha256(key.key))`; the human key lives only in `cache_key`. A key that is a prefix of
another (`go-list` `github.com/org/repo` vs `.../repo/v2`; `oci-tags` `r/team` vs `r/team/app`) would otherwise need one path to be both a file and a
directory, which `safe_path` (`src/storage/filesystem.rs:28-34`, `..` only) does not catch. The `_proxy_cache/{name}` prefix is kept for
`delete_prefix` (`src/api/repositories.rs:323-326`; a `remove_dir_all` of that directory, `src/storage/filesystem.rs:146-154`), which is why the
name is a validated segment (section 3): `/` in a name would nest `a/b` under `a`'s purge, `..` would 400 every cache write. `validate_spec` runs only
on writes and today's `create_repository` applies no name rule, so `db::kinds::check_repository_names(pool)` (`SELECT name FROM repositories` against
the rule) runs in `build_state` between `migrate()` and `init_repositories` (`src/server.rs:72-73`), bailing with every pre-upgrade offender for the
operator to rename by SQL. Legacy npm files (`src/proxy/mod.rs:113-116`) are orphaned: re-fetched once, removed by purge. `proxy_cache_meta` is no longer written.

```sql
-- src/db/migrations/013_proxy_cache_entries.sql
CREATE TABLE IF NOT EXISTS proxy_cache_entries (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    repository_id INTEGER NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
    kind TEXT NOT NULL, cache_key TEXT NOT NULL, status INTEGER NOT NULL,
    storage_path TEXT, content_type TEXT, etag TEXT, digest TEXT, size INTEGER NOT NULL DEFAULT 0,
    fetched_at TEXT NOT NULL DEFAULT (datetime('now')), expires_at TEXT,
    last_used_at TEXT NOT NULL DEFAULT (datetime('now')),
    UNIQUE(repository_id, kind, cache_key)
);
CREATE INDEX IF NOT EXISTS idx_proxy_cache_entries_expires ON proxy_cache_entries(expires_at);
CREATE INDEX IF NOT EXISTS idx_proxy_cache_entries_last_used ON proxy_cache_entries(last_used_at);
```

Appended to `migrate()` after `sql12` (`src/db/mod.rs:128-130`) with `?`, idempotent by construction (no ALTER). `ON DELETE CASCADE` works because
`connect()` sets `PRAGMA foreign_keys=ON` (`:79`). `expires_at NULL` = immutable; freshness is decided in SQL (`expires_at IS NULL OR expires_at >
datetime('now')`), never by `storage.exists` alone. `touch_entry` bumps `last_used_at` on every hit; `extend_ttl_secs` is `Some` only on a 304.

`ProxyEngine::fetch`:
1. `inflight.acquire("{member.id}/{kind}/{key}", singleflight_wait)` (section 3), then `get_entry`.
2. Fresh negative row (`status <> 200`, keyed by `cache_key(a)`) -> `Outcome::NotFound`.
3. Row `status 200`: `resolve_row(row)` is the one pointer helper, shared by steps 3, 5 and 7. A pointer row
   (`storage_path IS NULL AND digest IS NOT NULL`) is followed to `get_entry(store_key(a, digest))`; the result is the
   row whose file exists (the target for a pointer, the row itself otherwise) or `None` (row without file, file
   without row, evicted target). Fresh or Immutable + `Some(target)` -> *both* rows `touch_entry(id, None)`'d (an
   untouched pointer would be evicted under a hot tag), `target` served. `None` -> miss, upstream. `engine.rs` names no kind.
4. Build the request: `upstream_url`, `request_headers`, `If-None-Match` from a stale row's `etag` only when
   `resolve_row(stale)` is `Some` (a stale pointer whose target is gone re-GETs unconditionally, never serves a NULL
   `storage_path`); send through `send_with_auth` (section 7).
5. 304 -> `touch_entry(id, Some(ttl))` on the stale row and on its resolved target, serve the target. 200 -> read per
   `transfer`: Buffered loops `Response::chunk()` into `BytesMut` under `buffered_total` (no reqwest `stream` feature);
   Streamed `write_chunk`s into a `PartFile` opened once (section 3). Both hash the body, check `expected_sha256` and
   count bytes against `max_bytes`; the cap, a digest mismatch or a `verify_headers` refusal is `BadGateway` with
   nothing written. The file lands before any row, always by `rename`: Buffered opens its `PartFile` after the checks
   for one `write_chunk(&bytes)`; both `part.commit(storage, &cache_path(member, &store_key))`. Never `storage.put`
   (`fs::write`, `src/storage/filesystem.rs:113-120`, truncates in place): a buffered kind refreshed at its fixed path
   while `stream_response` reads it would serve a short body under the row's `Content-Length`; singleflight serializes
   writers, not readers. Then `upsert_entry` under `store_key(a, sha256)` plus, when it differs, the pointer row under
   `cache_key(a)`; `ttl_secs` from the policy (section 3).
6. `s.classify_status(a, status) == Miss` (default 404/410; OCI adds the post-token 401/403 of section 7) ->
   `upsert_entry(status, storage_path NULL, digest NULL, ttl = negative_secs)` under `cache_key(a)`, never `store_key`
   (no body, so no sha256; `store_key` is consulted only on a 200), i.e. the row step 1 reads back -> `NotFound`
   (`config.proxy.negative_cache_ttl`, `src/config.rs:146`, is finally read).
7. `Fail` or transport error -> stale `status 200` row with `resolve_row(stale) == Some(target)` -> `Cached { entry:
   target, stale: true }` + `Warning: 110` (a stale `oci-tag` pointer serves its manifest from disk: `upstream_503_serves_stale_tag`);
   else `BadGateway`. Failures are never cached.

Serving: `stream_response(&Payload)` = `Src::File` -> `read_stream` + `Body::from_stream(ReaderStream::new(reader))`, `Src::Bytes` -> the bytes,
`Src::HeadOnly` -> empty body; `Content-Length` from `size`, `Warning: 110` when `stale`. `read_stream` opens the file once, so a refresh's `rename` leaves
a reader on the old, complete inode (`buffered_refresh_never_truncates_reader`). A cold streamed blob reaches disk fully before the first byte goes out (no tee; a tee is a contained follow-up in `engine.rs`).

Purge: `proxy::purge::purge_repository(state, repo)`: Proxy -> `delete_entries(repo.id)` + `delete_legacy_meta(repo.id)` (`DELETE FROM
proxy_cache_meta ..`, as `src/api/repositories.rs:314-317`; that FK has no `ON DELETE`, `002_proxy_cache.sql:3`, so a pre-upgrade npm proxy would
otherwise still 500 on delete) + `delete_prefix("_proxy_cache/{name}")`; Group -> walk members (depth- and cycle-capped), purge every Proxy member;
Hosted -> 400. `POST .../purge-cache` accepts proxy and group (today proxy only, `src/api/repositories.rs:308`). Delete: `delete_repository`
(`:251-282`) keeps the packages guard, adds a 409 when the repo is still a group member (naming it), then `purge_repository`, then `DELETE FROM
repositories` (today the bare DELETE at `src/db/mod.rs:977-983` FK-violates into a 500 for any proxy with cache rows).

Eviction: the private `cleanup_proxy_cache` (`src/telemetry/cleanup.rs:135-150`) becomes `pub(crate) async fn sweep_proxy_cache(db, storage: &Arc<dyn
StorageBackend>, idle_days) -> anyhow::Result<SweepStats { rows, files, parts }>` over `evictable_entries(idle_days)` = `(status <> 200 AND expires_at
<= datetime('now')) OR last_used_at < datetime('now', '-' || ?1 || ' days')`: expired negatives go at once; every positive row, immutable or not, goes
after `idle_days` without a hit, the only bound on blob/crate/tarball/zip growth (no LRU by size), so it runs by default: `CleanupConfig` gets a
manual `Default` with `proxy_cache_older_than_days: Some(30)` (today `derive(Default)` -> `None`, `src/config.rs:164-170`). `run_cleanup` (`:35`)
becomes `pub(crate)`, returns `CleanupStats { prereleases: Option<u64>, proxy: Option<SweepStats> }`, runs the pre-release sweep only when `enabled`
and the proxy sweep whenever `proxy_cache_older_than_days > 0`; `start_cleanup_task` returns early (`:20-23`) only when neither applies. The same pass
calls `remove_stale_parts("_proxy_cache", 1 h)`. File first, then row; a pointer outliving its target is a miss (step 3). Both functions are
unit-tested in `cleanup.rs` (section 11), never through the 24 h loop. `[proxy]` keys are unchanged; README's `[cleanup]` block (`:248-251`) shows the
default.

## 6. OCI nested names

Every `/v2` route binds `{name}` to one segment (`src/server.rs:297-321`); validators, DB columns (`006_oci.sql`) and
`safe_path` already accept `team/app`. Only routing changes, with the `rewrite_go_module_path` trick: fold the name
into one percent-encoded segment that axum decodes back.

```rust
// src/registry/oci/routing.rs
pub fn rewrite_nested_name<B>(req: Request<B>) -> Request<B>;
fn split_v2_path(segs: &[&str]) -> Option<(usize, usize)>;
```

`split_v2_path` applies to paths starting with `/v2/` and longer than that. It scans markers from the right and keeps the rightmost match with the
exact arity of its endpoint: `[.., "tags", "list"]`; `[.., "blobs", "uploads", uuid_or_empty]` (tested before `blobs/{digest}`); `[.., "manifests",
reference]` (references contain `:`, never `/`); `[.., "blobs", digest]`. The name is everything between the repo segment and the marker; two or more
segments are joined with `%2F` and the URI re-parsed (query preserved; on failure keep the original and warn, as `src/server.rs:585-588`). Arity
disambiguates names literally called `blobs`/`manifests`/`uploads` (`/v2/r/team/blobs/blobs/uploads/` -> `team/blobs`). Ordering:
`decode_percent_encoded_slashes` (outermost, `main.rs:108, 114`) -> `MapRequestLayer(server::rewrite::pre_route)` (`src/server.rs:519-524`) -> layers
-> route match -> `auth_middleware`; the Go rewrite returns early on `/v2/` (`:550`), the OCI one touches only `/v2/`; `is_oci`
(`src/auth/middleware.rs:61`) is unaffected.

Storage path stability: `extract_image_name` (`oci/mod.rs:41-45`) and the manifest formula `format!("oci/{}/manifests/{}/sha256/{}", image_name, name,
hex)` (`:577, 753, 971`) move verbatim into `oci::paths::{image_name, manifest_path, blob_path}`: single-segment names yield byte-identical paths;
blob paths never contain the name (`:155, 244, 470, 963`). No data moves; `manifest_path_unchanged_for_single_segment_names` pins it.

## 7. OCI upstream

`upstream_url` = `{scheme}://{host}[/{prefix}]`; `OciUpstream::upstream_url` builds
`{scheme}://{host}/v2/{prefix/}{mapped}/{manifests|blobs|tags}/{selector}`. `mapped` = `library/{name}` when the host is
`registry-1.docker.io`, `index.docker.io` or `docker.io` (normalised to `registry-1`) and the name has one segment; else
`name`. A second opencargo is `http://127.0.0.1:{port}/oci-hosted`, giving `/v2/oci-hosted/{name}/...` for the e2e.
Artifacts: `OciArtifact::{Manifest { name, reference }, Tag { name, tag }, Blob { name, digest }, Tags { name }}`.
Manifest and Tag requests send `Accept: application/vnd.oci.image.{manifest,index}.v1+json,
application/vnd.docker.distribution.manifest.{v2,list.v2}+json` (Hub otherwise answers schema1 or 404 for multi-arch).
The upstream `Content-Type` is persisted and replayed; the client's Accept never picks a variant (as hosted today,
`oci/mod.rs:586`).

Tag resolution: `Tag` is one upstream GET through the generic hooks: `store_key(Tag, hex) = oci-manifest:sha256/{hex}` (Immutable; `verify_headers`
compares the upstream `Docker-Content-Digest`, when present, with the body hash, mismatch -> `BadGateway`, nothing stored) and `cache_key(Tag) =
oci-tag:{name}/{tag}` is the pointer row (Ttl(Default)). A fresh tag row serves the manifest locally; a stale one re-GETs (`If-None-Match` when the
upstream gave an ETag). Index children are pulled by digest through the same `Manifest` path (multi-arch without index walking). `refs::extract_refs`
extends `extract_blob_digests` (`oci/mod.rs:665-684`) with `manifests[]` for hosted pushes.

Token dance (`proxy/auth.rs`, RFC 6750): `send_with_auth` attaches a cached token for `(realm, scope)`, else the configured static auth. On 401 with
`Www-Authenticate: Bearer realm=..,service=..,scope=..` and a strategy `bearer_scope` (OCI: `repository:{mapped}:pull`): parse, run `realm` through
`validate_upstream_url` (upstream-controlled data), GET `{realm}?service=..&scope=..`. `Authorization: Basic` from `UpstreamAuth::Basic` (lifts Hub's
anonymous limits) goes along only when the realm's host and port equal `up.base`'s or the realm is in `up.token_realms`; any other realm is queried
anonymously (a hostile realm would otherwise collect the credentials). Read `token`/`access_token` and `expires_in` (default 300 s, cached to 90 %),
retry once with `Bearer`. A realm refusing a token, or a 401 without `bearer_scope`, is `BadGateway`. A 401/403 *after* a freshly issued token is how
Hub, GHCR and Quay answer an unknown or private repository (they never 404 a repository, only a tag), so `OciUpstream::classify_status` maps it to
`Miss` for artifacts with a `bearer_scope`: the client gets `manifest unknown`, a group member is skipped silently, the negative row expires after
`negative_secs`. Hub blob 307s follow the existing redirect policy (`src/proxy/mod.rs:76-88`).

Credentials: `[[repositories]] upstream_auth = { type = "basic", username = "..", password = ".." }` or `{ type = "bearer", token = ".." }`, or
`OPENCARGO_UPSTREAM_AUTH_<REPO>=basic:user:pass | bearer:token` (env wins; `<REPO>` = the name uppercased, every `[^A-Za-z0-9]` byte -> `_`:
`npm-proxy` -> `OPENCARGO_UPSTREAM_AUTH_NPM_PROXY`; two configured names mangling alike is a startup error), plus `token_realms =
["https://auth.docker.io/token"]` (that exact default for the Hub aliases above, empty otherwise). `build_state` scans `std::env::vars()` after
`load_config` (`config.rs` has no env machinery; every override today is a clap `env =` in `src/main.rs:13-21`) and loads both into
`AppState.upstream_auth`; the admin API neither accepts nor returns them. HEAD: a cached manifest or blob is answered from its row (section 3); a
manifest miss GETs and warms the cache; a blob miss forwards an authenticated upstream HEAD and stores nothing.

Isolation: proxied artifacts never enter the `oci_*` tables (decision 5), so a proxy repo deletes cleanly; a layer both
hosted and proxied in one group is stored twice. The 401 shaping toward docker (`src/auth/middleware.rs:194-213`) is unchanged.

## 8. Cargo and Go upstream

Cargo. `upstream_url` = a sparse index base (`https://index.crates.io`, or a second opencargo's `http://host:port/cargo-up/index`). `Config` ->
`{base}/config.json`; `Index { name }` -> `{base}/{compute_prefix(name)}/{name.to_lowercase()}` (`cargo/mod.rs:30-41`); `Crate { name, version, dl,
cksum }` -> `expand_dl_template(dl, ..)` replacing `{crate}`, `{version}`, `{prefix}`, `{lowerprefix}`, `{sha256-checksum}`, or appending
`/{crate}/{version}/download` when no marker is present (cargo's rule; crates.io's `dl` has no markers). The `dl` host is chosen by upstream
*content*, not by the admin, so the expanded URL goes through `validate_upstream_url` (section 10) *and* `is_blocked_ip` (`src/proxy/mod.rs:29-45`, IP
literals only, so `static.crates.io` costs nothing): a compromised sparse index cannot aim downloads at `http://10.x.y.z/...` or
`http://127.0.0.1:2379/...` and have the body cached and served back. The per-repo opt-out `dl_allow_private = true` (`[[repositories]]` or
`OPENCARGO_DL_ALLOW_PRIVATE_<REPO>=1`, config/env only like `upstream_auth`, carried by `Upstream`) skips the literal check; every cargo proxy test
sets it (`ProxyOpts::dl_allow_private`, section 11) because it fronts a second opencargo whose `dl` is `http://127.0.0.1:{port}/...`
(`cargo/mod.rs:93-96`). Index lines are served verbatim; only our `config.json` is generated. Cargo sends the token verbatim (README's `token =
"Bearer trg_..."` stays); `-`/`_` are not folded.

Go. `upstream_url` = a GOPROXY base (`https://proxy.golang.org`, `http://host:port/go-up`). `List` -> `{base}/{module}/@v/list`, `Latest` ->
`/@latest`, `Info|Mod|Zip` -> `/@v/{version}.{info,mod,zip}`, `module` escaped as received. `escape.rs` implements `escape`/`unescape` per
`golang.org/x/mod/module` (`A` <-> `!a`, `!` <-> `!!`), `validate_escaped_module` (the `go` name rule plus `!` followed by `[a-z]` or `!`) and
`is_canonical_version` (`v` + semver core, optional pre-release/build, pseudo-versions included). `.zip` `max_bytes` 512 MiB, `.mod` 16 MiB. Status
mapping is load-bearing for `GOPROXY=a,b,direct`: 404/410 -> negative entry -> 404 (go moves on); anything else -> 502 (go stops instead of silently
falling to `direct`). Hosted fixes bundled: RFC 3339 `Time`, semver-max `@latest` (`go/mod.rs:85-87`), `@v/list` 404 for an unknown module (section
4), uppercase hosted modules via `unescape`. sumdb is out of scope; e2e uses `GOSUMDB=off`, `GOFLAGS=-mod=mod`; `README.md:210`'s `GONOSUMCHECK`
becomes `GONOSUMDB`.

## 9. OSV

Config: `VulnScanConfig { enabled, block_on_critical, osv_base_url: String, fail_closed: bool, max_concurrency: usize }` (`src/config.rs:30-35`) drops
`derive(Default)` for a manual `impl Default` = `{ enabled: false, block_on_critical: false, osv_base_url: "https://api.osv.dev".into(), fail_closed:
false, max_concurrency: 8 }`, applied per field by `#[serde(default)]`. Under the derive a config with no `[vuln_scan]` table (`:22-23`) gets
`osv_base_url == ""` (`Url::parse` fails in `VulnScanner::new`: no server) and `max_concurrency == 0` (`Semaphore::new(0)` never grants: `advisories`
hangs). `OPENCARGO_OSV_BASE_URL` overrides like `OPENCARGO_BASE_URL` (`src/main.rs:20-22, 49-51`). `VulnScanner::new(&cfg)` replaces `new(enabled)`
(`src/server.rs:164`, `src/telemetry/vulns.rs:150`). Fields, manual `Default`, `new(&cfg)` and the `assess`/`persist` split are foundation commit 4
(`scan_version` = `assess` then `persist`, severity logic untouched); Agent OSV replaces the bodies behind them;
`vuln_test::default_config_has_osv_url_and_concurrency` pins the defaults.

```rust
// src/telemetry/vulns/osv.rs
pub struct OsvClient { http: reqwest::Client, base: reqwest::Url, sem: Arc<Semaphore>, cache: AdvisoryCache }
pub struct Advisory { pub id: String, pub summary: Option<String>, pub severity: Severity, pub score: Option<f64> }
impl OsvClient {
    pub async fn query_batch(&self, ecosystem: &str, deps: &[(String, String)]) -> Result<Vec<Vec<String>>, ScanError>;
    pub async fn advisories(&self, ids: &[String]) -> HashMap<String, Result<Arc<Advisory>, String>>;
}
// src/telemetry/vulns/severity.rs
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, serde::Serialize)] pub enum Severity { Unknown, Low, Medium, High, Critical }
pub fn classify(id: &str, database_specific: Option<&serde_json::Value>, entries: &[OsvSeverityEntry]) -> (Severity, Option<f64>);
// src/telemetry/vulns/mod.rs
pub struct VulnScanner { osv: Option<OsvClient> }
impl VulnScanner {
    pub fn new(cfg: &VulnScanConfig) -> anyhow::Result<Self>;
    pub async fn assess(&self, metadata_json: &str, ecosystem: &str) -> Result<ScanResult, ScanError>;
    pub async fn persist(&self, db: &SqlitePool, version_id: i64, r: &ScanResult) -> Result<(), sqlx::Error>;
    pub async fn scan_version(&self, db: &SqlitePool, version_id: i64, metadata_json: &str, ecosystem: &str) -> Result<ScanResult, ScanError>;
}
```

`assess` (pure, no DB): `deps::extract_dependencies` (from `vulns.rs:270-331`; the `"Go"` arm now parses go.mod `require` lines and blocks, since
`go/mod.rs:296-305` stores raw go.mod and the JSON branch never matched) -> `query_batch` (`POST {base}/v1/querybatch`, positional mapping
bounds-checked as `vulns.rs:188-194`) -> unique ids -> `advisories`: a `JoinSet` gated by `Semaphore(max_concurrency)`, each task consulting
`AdvisoryCache` (`Mutex<HashMap<String, Arc<Advisory>>>`, 10k entries, FIFO) before `GET {base}/v1/vulns/{id}` (15 s timeout); a per-id failure yields
`Severity::Unknown`, uncached, the finding still reported. `query_batch` failure -> `ScanError::Upstream`. `classify` precedence: `MAL-` id prefix ->
Critical, score None; `database_specific.severity` label (`CRITICAL|HIGH|MODERATE|MEDIUM|LOW`, case-insensitive); else entries typed `CVSS_V4` then
`CVSS_V3` via `cvss::Cvss::from_str(vector)` (`score()` -> `f64`, `severity()` -> label, CVSS 3.0/3.1/4.0 in one arm), take the max; `CVSS_V2` and
unparsable vectors -> Unknown. `ScanResult.status` is `critical` iff any Critical, `warning` iff any finding, else `clean`. `VulnDetail` gains `score:
Option<f64>`; `severity` becomes a non-optional lowercase label, `"unknown"` without CVSS data (today a vector or `null`, `vulns.rs:78, 196-215`);
`summary` comes from the full record. Frontend: `fetchVulns` (`frontend/src/core/api.ts:251-255`) casts the raw body to `VulnReport` (`types.ts:227-232`:
`package_name`, `vulnerabilities`), but the handler answers `{package, version, scanned_at, total_deps, vulnerable_deps, status, details}`
(`src/api/vulns.rs:114-122`, `details` = `VulnDetail[]` or `null`), so `vd().vulnerabilities.length` (`PackageDetail.tsx:396`) throws when the Security
tab loads and the unguarded `vuln.severity.toLowerCase()` at `:465` is dead code. Agent OSV adds `frontend/src/core/vulns.ts`: `VulnsResponse` (the
handler's shape), `toVulnReport(raw)` (`package` -> `package_name`; `details ?? []` -> `{ id: vuln_id, severity, score: score ?? null, title: summary,
description: "{dependency}@{version}", fixed_in: null }`) applied inside `fetchVulns`/`rescanVulns`, and `severityChip(severity: string | null |
undefined)` (`SEVERITY_CHIP` moved from `:27-33`, gains `unknown`; a pre-upgrade vector or `null` -> `chip-neutral`) used at `:465`; `VulnEntry.severity`
becomes `string | null`, plus `score: number | null`. `vulns.test.ts` (vitest, `frontend/vitest.config.ts:6`) feeds one verbatim backend payload (a vector
row, a `null` row, a `"critical"` row) through `toVulnReport`, asserting `vulnerabilities.length`, each `id`/`title`, `severityChip` per entry, and `details: null` -> `[]`.

Publish gate. Today `finalize_publish` runs after `storage.put` and `create_version` (`src/registry/mod.rs:339-350` vs `npm/mod.rs:239-256`,
`cargo/mod.rs:299-360`, `go/mod.rs:337-362`), so a 400 leaves the version served. New order: validate body -> `publish_gate(&state, Format::X,
&metadata_json).await?` -> first write. With `block_on_critical` the gate awaits `assess`; Critical -> `BadRequest("publish blocked: critical
vulnerabilities found in dependencies")`, nothing written; OSV failure -> `Ok(PreScan(None))` + warn when `!fail_closed`, else `ServiceUnavailable`.
`finalize_publish` `persist`s the `PreScan` once the version row exists or, when `None`, spawns the background scan as today (`:351-361`). The
`status` CHECK (`009_vulns.sql:8`) is unchanged: `error` is never persisted.

## 10. Error policy

`AppError::BadGateway(String)` -> 502 (`src/error.rs:52-59` gains the arm). `NotFound` = authoritatively absent (hosted
miss, fresh negative entry, upstream `classify_status == Miss`: 404/410, plus OCI's post-token 401/403). `BadGateway`
= upstream unreachable, 5xx, a realm refusing a token, 401 without a challenge, 403 outside OCI, 429, size cap, digest
mismatch, corrupt cache file, with no stale copy. `ServiceUnavailable` stays for our own DB/OSV faults; `Internal` is
no longer used for upstream conditions.

Resolver: member not found -> warn, skip; `ensure_can_read` Unauthorized/Forbidden -> silent skip (a private member is invisible; the group ends in
404, not 403, as `tests/pnpm_e2e_test.rs:415-418` asserts), other errors propagate; wrong format -> warn, skip; already seen -> skip; depth > 5 ->
`Internal("group nesting depth exceeded")`; empty member list -> warn + `NotFound` (legacy rows degrade to 404 instead of today's 500). Leaf
`NotFound` (outcome or error) -> next; any other `Err` -> first one remembered and logged, walk continues. `first_hit`: first `Found` wins; none +
failure -> `BadGateway("group {url}: member {m} failed: {e}")`; none -> `NotFound`. `collect`: `hits` in member order; zero hits + failure ->
`BadGateway`; hits + failure -> `degraded: Some(msg)` and the handler adds `Warning: 199 - "member {m} unavailable"`; empty without failure -> `Ok`
(cargo index 404, go `@v/list` 404, tags `[]`).

Client mapping: go moves to the next GOPROXY entry on 404/410 only and stops on 502; cargo hard-fails either way with a
distinct message; docker reports `manifest unknown` on 404 and retries 5xx; npm/pnpm surface E404 vs a fetch error. OCI
error bodies keep `AppError`'s `{ "error": .. }` JSON (spec-shaped bodies: follow-up).

Admin API and seed: `validate_spec` refusals are 400 from the API and `anyhow::bail!` from `init_repositories` (replacing `src/db/mod.rs:157-165`,
`src/api/repositories.rs:76-81`); `update_repository` (`:185-247`) builds a `RepoSpec` from the stored row's `kind`/`format` merged with the patch
(`UpdateRepositoryRequest` has only `visibility`, `upstream`, `members`, `:35-39`) and validates it like create; `validate_spec` refuses `upstream`
outside Proxy and `members` outside Group (today the handler writes them onto any repo unchecked). `validate_upstream_url` (`src/proxy/mod.rs:49-62`)
additionally refuses link-local literals (`169.254.0.0/16`, `fe80::/10`) and `0.0.0.0`; loopback and RFC 1918 stay allowed for the admin-chosen
upstream (local mirrors, tests); upstream-chosen URLs (cargo `dl`, redirect hops) are held to `is_blocked_ip` (section 8). Writes on non-hosted repos
are 400 via `ensure_hosted`.

## 11. Test matrix

Harness (`tests/common/`, `#![allow(dead_code)]` because CI runs clippy with `-D warnings` on all targets, `ci.yml:54`): - `spawn_server(SpawnOpts {
anonymous_read, repositories: Vec<RepositoryConfig>, proxy, vuln })` -> `TestServer { base_url, port, handle, tmp }`, built like
`tests/pnpm_e2e_test.rs:30-98`, ALWAYS wrapped with `.map_request(server::decode_percent_encoded_slashes)` (parity with `main.rs:108, 114`). Builders
`hosted(name, fmt, vis)`, `proxy(name, fmt, upstream)` = `proxy_with(.., ProxyOpts::default())`, `proxy_with(name, fmt, upstream, ProxyOpts {
dl_allow_private: bool, upstream_auth: Option<UpstreamAuth>, token_realms: Vec<String> })` filling the `RepositoryConfig` fields of sections 7-8 (no
env in tests), `group(name, fmt, members)`. `expire_entries(&server)` opens `tmp/opencargo.db`, runs `UPDATE proxy_cache_entries SET expires_at =
datetime('now', '-1 second') WHERE expires_at IS NOT NULL`: TTL expiry is tested by expiring and counting tap hits, never by sleeping (a 1 s TTL is
racy by construction). - `upstream_tap::start(target) -> Tap { base_url, hits: Arc<Mutex<Vec<(Method, String)>>>, fail: Arc<AtomicBool> }`: a
recording reverse proxy in front of a second real opencargo; `fail` answers 503. - `fake_upstream/{npm,cargo,go,oci}.rs` (one file per owner; `mod.rs`
is four `pub mod` lines), what a second opencargo cannot emulate: a Bearer challenge (on HEAD too; `/token` optionally requiring Basic), a Hub-shaped
401 (token issued, then `UNAUTHORIZED` for an unknown name), `library/` names, a 300 MiB streamed blob, ETag/304 stubs, a `slow` stub for
singleflight, a `drip` stub (one chunk every 500 ms for 8 s) for timeouts, crates.io-style `config.json` with and without `dl` markers and with a
private-literal `dl`, a GOPROXY answering 410 for one version. - `fake_osv.rs`: `POST /v1/querybatch`, `GET /v1/vulns/{id}`, per-id counters,
in-flight latch. - `run_cmd(program, args, cwd, env: &[(&str, &OsStr)])` from `tests/pnpm_e2e_test.rs:107-129` (the pnpm-only `HOME`/`XDG_*`/
`npm_config_store_dir` block, `:114-121`, becomes the caller's `env`); `client_bin("CARGO_BIN"|"GO_BIN"|"DOCKER_BIN"|"PNPM_BIN")`: an absent binary
prints `skipped:` and returns, unless `OPENCARGO_E2E_REQUIRE=1` (CI) makes absence a failure. Seeding helpers `build_tarball`/
`build_npm_publish_body` (`npm_test.rs:13-70`, duplicated at `features_test.rs:17-81`), `build_go_module_zip`/`publish_go_module`
(`go_test.rs:17-44, 114-140`), `sha256_digest`/`push_blob` (`oci_test.rs:84-133`), `basic_auth_header`/`create_user` (`docker_e2e_test.rs:99-167`).
All of it lands in `tests/common/mod.rs` in foundation commit 5, with the six existing files rewired onto it, so the parallel agents only add cases.

| format | hosted | proxy (tap or fake) | group hosted+proxy | permission filtering | negative / failure | real client (2nd instance upstream) |
|---|---|---|---|---|---|---|
| npm | `npm_test.rs` (exists) + `unscoped_dist_tags_get_put_delete` (commit 1) | `npm_proxy_test.rs`: `proxy_serves_packument_and_tarball_from_second_instance`, `tarball_urls_point_at_proxy`, `packument_ttl_expiry_refetches` (`expire_entries`, then exactly one more tap hit), `packument_etag_304_touches_row`, `tarball_immutable_one_upstream_hit` | `group_serves_hosted_first`, `group_falls_through_to_proxy`, `group_tarball_url_points_at_group`, `group_dist_tags_via_proxy_member`, `search_recurses_nested_groups` | `group_hides_private_member_returns_404` | `missing_package_negative_cached_one_hit`, `upstream_503_serves_stale_metadata`, `group_upstream_failure_is_502_not_404` | `pnpm_e2e_test.rs` (exists) + `pnpm_install_through_group_over_second_instance` |
| cargo | `cargo_test.rs` (moved from `features_test.rs:83-101, 676-913, 983-1010` in commit 5) | `cargo_proxy_test.rs`: `proxy_index_and_download_from_second_instance`, `download_verifies_cksum_from_index` (mismatch -> 502, nothing stored), `dl_template_markers_and_default_expand`, `index_etag_revalidation_touches_row`, `prefix_routes_for_1_2_3_4_char_names`, `config_json_anonymous_on_private_repo_points_at_requested_repo_with_auth_required` (`anonymous_read = false`, no credentials sent, 200) | `group_config_json_dl_points_at_group`, `group_index_unions_hosted_and_proxy_lines`, `group_download_first_member_wins`, `yank_on_group_is_400` | `group_hides_private_member` | `unknown_crate_negative_cached`, `upstream_503_is_502`, `stale_index_served_on_upstream_error`, `dl_host_on_private_literal_is_refused_without_optin` (fake index with `dl = http://127.0.0.1:1/`, plain `proxy(..)` so `dl_allow_private = false`: 502, no row, tap never hit) | `cargo_e2e_test.rs`: `cargo_publish_then_fetch_through_group` (`cargo publish --registry` to A/hosted; `cargo fetch` + `cargo build --offline` via A/group whose proxy member fronts B; `CARGO_HOME`, `CARGO_TARGET_DIR` isolated), `cargo_fetch_private_group_with_token_only_in_cargo_home` (A spawned with `anonymous_read = false`, private A/group; the only credential is `CARGO_HOME/credentials.toml`; proves the anonymous `config.json` bootstrap through `auth_middleware`) |
| go | `go_test.rs` (exists) + `info_time_is_rfc3339` | `go_proxy_test.rs`: `proxy_list_info_mod_zip_bytes_exact`, `escaped_uppercase_module_roundtrip`, `non_canonical_version_is_ttl_not_immutable`, `list_and_latest_ttl_expiry` (`expire_entries`, tap hit counts) | `group_list_union_and_latest_max_semver`, `group_zip_first_member_wins`, `unknown_module_list_is_404_known_empty_is_200` | `group_hides_private_member` | `unknown_module_negative_cached_no_second_hit`, `upstream_410_is_404`, `upstream_503_is_502_not_404` | `go_e2e_test.rs`: `go_mod_download_and_build_through_group` (`GOPROXY=A/group`, `GOSUMDB=off`, isolated `GOPATH/GOMODCACHE/GOCACHE`) |
| oci | `oci_test.rs`, `docker_e2e_test.rs` (exist) | `oci_proxy_test.rs`: `proxy_pull_by_tag_then_by_digest_from_second_instance` (2 rows, 1 download, second pull never hits the tap), `hub_token_dance_with_fake_registry` (token fetched once for two pulls), `accept_header_sent_upstream`, `library_prefix_only_for_docker_hub_hosts`, `blob_larger_than_buffer_cap_streams_through` (300 MiB, size == Content-Length), `blob_digest_mismatch_is_502_nothing_stored`, `slow_drip_blob_past_buffered_total_completes` (`connect_timeout=1s`, so `buffered_total` 6 s; the 8 s drip streams through), `tag_revalidation_updates_after_upstream_retag`, `head_blob_miss_forwards_head_without_download` (tap: one HEAD, no GET, through the challenge), `head_blob_hit_is_served_from_cache_without_upstream` (pull, then HEAD every layer with the tap failing: 200s, zero tap requests), `second_puller_proceeds_after_singleflight_wait`, `tags_list_ttl` | `group_manifest_blob_tags_through_group`, `oci_group_location_and_digest_headers_use_group`, `push_to_group_is_400` | `group_hides_private_member` (401 shape keeps `Www-Authenticate: Basic`) | `unknown_manifest_negative_cached` (fake 404), `unknown_repository_401_after_token_is_404_negative_cached` (fake registry issues a token then 401s the unknown name: client 404, exactly one negative row, second request never hits the fake), `upstream_503_serves_stale_tag`, `token_realm_on_link_local_is_refused`, `token_realm_off_upstream_host_gets_no_credentials` (fake `/token` on another port demands Basic: 502, tap shows no `Authorization`), `oci_proxy_refuses_client_delete`, `oci_proxy_purge_removes_rows_and_files_and_leaves_oci_tables_untouched` | `docker_cli_e2e_test.rs`: `docker_push_nested_then_pull_through_proxy_and_group` (`docker login/push 127.0.0.1:P/oci-hosted/team/app:1.0` on B, `docker rmi`, `docker pull` via A proxy and A group) |
| nested names | `oci_nested_test.rs`: `push_pull_team_app_and_org_team_app_on_every_route`, `manifest_path_unchanged_for_single_segment_names`, `location_headers_carry_nested_name`, `name_segment_called_blobs_or_manifests_routes`; unit `routing::six_route_shapes` | | | | | covered by the docker CLI e2e |
| resolver / admin | `group_resolver_test.rs`: `depth_cap_and_cycle_detected`, `member_format_mismatch_skipped`, `create_update_seed_validate_members_and_upstream`, `repo_name_rule_refused_on_create_and_seed` (`A`, `a/b`, `a..b`, 65 chars: 400 / bail), `empty_member_list_refused_on_create`, `delete_then_recreate_never_serves_old_cache`, `delete_proxy_with_legacy_proxy_cache_meta_row` (seeds a `proxy_cache_meta` row by SQL, expects 204), `delete_member_of_group_is_409`, `purge_group_purges_proxy_members_only`, `migrate_013_twice_is_noop` (the eviction cases are unit tests in `cleanup.rs`, below: `run_cleanup` and `sweep_proxy_cache` are private to the crate and the 24 h loop has no completion signal); `permissions_test.rs:624-656` is reordered so `cargo-hosted-ok` and a new `npm-hosted-ok` come first, `body["members"] = json!([])` (`:646`, which today applies to every group row) becomes `json!([hosted member of that format])`, `cargo-group` / `oci-proxy` / `go-proxy` flip to 201, and new rows expect 400 for a pypi proxy, an unknown member, a cross-format member and `members: []` | | | | | |
| osv | `vuln_test.rs` (offline, fake OSV): `mal_id_is_critical`, `database_specific_label_wins`, `cvss_v3_9_8_is_critical`, `cvss_v4_vector_is_scored`, `cvss_5_3_is_medium_not_blocking`, `advisory_fetched_once_across_two_scans`, `never_more_than_max_concurrency_in_flight`, `blocked_publish_leaves_nothing_downloadable` (400; packument, tarball and version row absent), `non_blocking_publish_persists_critical_row` (polled), `osv_down_fail_open_publishes_with_warning`, `osv_down_fail_closed_is_503`, `go_require_block_is_scanned`, `default_config_has_osv_url_and_concurrency` (`toml::from_str::<Config>("")`) | | | | | |

In-crate unit tests: `kinds::{roundtrip_matches_check_constraints, check_repository_names_refuses_pre_upgrade_slash}` (raw `INSERT` of `a/b`, the check bails naming it), `resolve::error_policy_table` (a scripted `Leaf`),
`engine::{prefix_keys_do_not_collide, singleflight_one_upstream_hit, singleflight_wait_timeout_proceeds_unlocked,
part_file_unlinked_on_drop_and_cap` (drop, then `std::fs::metadata` on the resolved path fails at once),
`part_file_commit_keeps_file, buffered_refresh_never_truncates_reader` (a `read_stream` held across an expire + longer re-fetch drains the old
length, byte-exact), `pointer_row_follows_store_key, pointer_row_touched_with_target` (both `last_used_at` move), `stale_pointer_served_on_upstream_error,
stale_pointer_without_target_regets_without_if_none_match, buffered_total_timeout_is_502, head_via_get_shares_singleflight_key, head_hit_answers_from_row_without_request, head_on_negative_row_is_404}`,
`server::env_repo_key_mangles_and_refuses_collisions` (`npm-proxy`, `npm.proxy`),
`proxy_cache::roundtrip_and_freshness`, `auth::parse_bearer_challenge_variants`,
`cleanup::{sweep_evicts_idle_immutable_and_expired_negatives_keeps_stale_positive, sweep_reclaims_abandoned_part_file,
run_cleanup_with_enabled_false_still_sweeps_proxy}` (beside `cleanup_spares_go_pseudo_versions`, `cleanup.rs:162-218`),
`cargo::upstream::{expand_dl_template, dl_private_literal_refused_unless_allowed}`, `go::escape::roundtrip_and_validation`,
`oci::routing::six_route_shapes`, `oci::upstream::classify_status_post_token_401_is_miss`, `severity::classify_precedence`.

Network gating: `tests/proxy_test.rs` (live npmjs.org, `:128`) returns early with `skipped:` unless
`OPENCARGO_NETWORK_TESTS=1`, from foundation commit 4; `tests/vuln_test.rs`'s live osv.dev cases (`:165-275`) get the
same gate from Agent OSV, who rewrites that file. CI and tooling (integration commit, `ci.yml` + `Makefile` only):
`ci.yml` adds `actions/setup-go@v5` (go 1.22) and sets `OPENCARGO_E2E_REQUIRE=1` on the test step (cargo, docker, pnpm
are already on the runner). Makefile: `test-quick` adds `*_proxy_test` and `vuln_test`; new `test-e2e-cargo`,
`test-e2e-go`, `test-e2e-docker`, `test-network`.

## 12. Delivery plan

Every commit keeps the CI gate green: `cargo clippy --all-targets --all-features -- -D warnings` (`ci.yml:54`), `cargo test`.

Sequential foundation (one agent, branch `feat/proxy-group-all-formats`); `foundation-ready` = tip of commit 5:
1. `refactor(registry): typed repo kind/format, per-format route factories`. `src/db/kinds.rs` + `config.rs` aliases;
   `Repository::kind()/fmt()/members()`; every `repo_type`/`format` string compare (`src/registry/mod.rs:102`,
   `npm/mod.rs:382, 554, 640, 774, 854`, `cargo/mod.rs:461, 512`, `oci/mod.rs:334, 407`, `api/promote.rs:130, 137`,
   `api/repositories.rs:61-81, 308`, `db/mod.rs:145-157`) switched to enums; `Format::osv_ecosystem()`;
   `registry::load_repo`; `npm/cargo/go/oci::routes()`; `go/routes.rs` receives `version_dispatch` and
   `rewrite_module_path`; `oci/{paths,refs,routing}.rs` (routing = identity) composed in `server/rewrite.rs::pre_route`;
   OCI deletes gain `ensure_hosted`; `auth_middleware` gains `is_cargo_config` and `config_json` its format/read gate, together (section 4);
   `npm/routes.rs` adds the unscoped `/{repo}/-/package/{name}/dist-tags` and `.../dist-tags/{tag}` routes (section 4). Tests
   `kinds::roundtrip_matches_check_constraints`, `npm_test::unscoped_dist_tags_get_put_delete`, `features_test::config_json_tokenless_gate`
   (`anonymous_read = false`: 200 on a private cargo repo, 400 on an npm repo, 403 with a token lacking read; to `cargo_test.rs` in commit 5).
2. `feat(proxy): cache entries table, streamed storage, BadGateway`. `AppError::BadGateway`; `storage::{rename,
   read_stream, remove_stale_parts}` + `FilesystemStorage::resolve`; `db/proxy_cache.rs` + migration 013; `Cargo.toml`
   promotes `tokio-util` (`io`) and `semver` to direct (both transitive today: `Cargo.lock`'s `opencargo` dependency list gains them). Test `proxy_cache::roundtrip_and_freshness`.
3. `feat(proxy): ProxyEngine, strategies, bearer auth, purge`. `proxy/{strategy,engine,singleflight,auth,purge}.rs`;
   `AppState.{proxy,upstream_auth}`; `config.rs` `upstream_auth`, `token_realms`, `dl_allow_private` + the env scan in
   `build_state` (section 7); `negative_cache_ttl` read. `RepositoryConfig` has no `Default` (`src/config.rs:176-184`) and
   is built by exhaustive literals in 33 places across 17 test files, which `#[serde(default)]` does not save, so this
   commit derives `Default` on it (`RepoKind::Hosted`/`Format::Npm` are the `#[default]` variants and `Default` is in both derive
   lists since commit 1, section 3; `Visibility` has one, `:210`) and rewrites every literal to `..Default::default()`: `tests/{auth,authz,deps,docker_e2e,e2e_scoped,features,go,
   npm,oci,permissions,pnpm_e2e,promote,proxy,tls,vuln,webhook,ws}_test.rs` are in scope. `ProxyClient` survives until
   commit 4. Tests `engine::*`, `auth::*`, `server::env_repo_key_*`.
3b. `test(proxy): shared harness, offline npm proxy cases`. `tests/common/{mod,upstream_tap}.rs` (`spawn_server`,
   builders, `ProxyOpts`, `expire_entries`, `Tap`; section 11) and `tests/npm_proxy_test.rs` with the cases that hold on
   `ProxyClient` and on the engine alike: `proxy_serves_packument_and_tarball_from_second_instance`,
   `tarball_urls_point_at_proxy`, `tarball_immutable_one_upstream_hit`, `group_falls_through_to_proxy`. Rationale: the
   only in-tree test with `upstream: Some(..)` is `tests/proxy_test.rs:128` (live npmjs.org), which commit 4 gates off
   CI, and `pnpm_e2e_test.rs`'s `npm-all` group has one hosted member (`:275-290`); without 3b the proxy path would be
   rewritten with no runnable assertion.
4. `refactor(npm): resolver, publish gate, npm on the engine`. `registry/{resolve,publish}.rs` (`finalize_publish` from
   `mod.rs:298-363`; the gate order of section 9 applied to npm, cargo, go is this commit's one intended behaviour
   change, visible only under `block_on_critical`); npm reads rewritten onto `first_hit`/`collect` + `NpmUpstream` with
   `Cx.failure = FailurePolicy::NotFound`, a one-commit flag keeping today's blanket 404 so `npm_test`, `pnpm_e2e_test`,
   `proxy_test` and the 3b cases pass unchanged (`proxy_test.rs` only gains the `OPENCARGO_NETWORK_TESTS` early return);
   `npm_proxy_test.rs` gains `missing_package_negative_cached_one_hit`, `packument_ttl_expiry_refetches` and
   `upstream_503_serves_stale_metadata` (they read `proxy_cache_entries`, meaningful only on the engine); search and
   dist-tags keep today's semantics; `ProxyClient`, `db::*proxy_cache_meta*` and the legacy npm cache layout deleted;
   `telemetry/vulns.rs` -> `vulns/mod.rs` with `new(&VulnScanConfig)`, `assess`/`persist`, the three config fields with
   their manual `Default` and `--osv-base-url` (section 9); `CleanupConfig::default`, `run_cleanup`/`sweep_proxy_cache` (section 5). Tests
   `resolve::error_policy_table`, `cleanup::*`.
4b. `feat(npm): 502 on upstream failure`. Removes `FailurePolicy` (every caller is `BadGateway`), updates
   `test_group_repo_falls_through_to_proxy` / `test_proxy_handles_nonexistent_package` (`proxy_test.rs:418, 455`),
   adds `group_upstream_failure_is_502_not_404` to `npm_proxy_test.rs` and the CHANGELOG line of section 13, in one commit.
5. `feat(api): validate_spec, purge/delete for proxy and group, shared test harness`. `validate_spec` on create/update
   and `init_repositories`; `purge_repository` wired to purge and delete (409 on group membership, legacy meta cleared);
   `check_repository_names` in `build_state` (section 5); `permissions_test.rs:624-656` reordered and flipped (section 11);
   `tests/group_resolver_test.rs` on the 3b harness; `fake_upstream/mod.rs` = four `pub mod` lines over empty stubs, `tests/common/fake_osv.rs`
   an empty stub declared in `mod.rs` likewise (Agent OSV fills it without touching the frozen file); cargo cases and helpers move from `features_test.rs`
   to `tests/cargo_test.rs`, so no test file has two owners; `tests/common/mod.rs` is completed with `run_cmd`, `client_bin`
   and the seeding helpers of section 11, and `npm/go/oci/docker_e2e/pnpm_e2e/features_test.rs` are rewired onto them, so
   `cargo_e2e_test.rs`, `go_e2e_test.rs` and `docker_cli_e2e_test.rs` need nothing from the frozen file. = `foundation-ready`.

Parallel phase (five worktrees off `foundation-ready`, disjoint ownership):
- Agent NPM owns `src/registry/npm/**`, `tests/{npm,npm_proxy,pnpm_e2e,proxy}_test.rs`, `fake_upstream/npm.rs`, README
  "npm / pnpm / yarn" (`README.md:167-175`); extends the `npm_proxy_test.rs` of 3b/4/4b. Commits: `feat(npm): dist-tags through proxy and group, search recurses
  nested groups`, `test(npm): tap and second-instance cases`.
- Agent CARGO owns `src/registry/cargo/**`, `tests/{cargo,cargo_proxy,cargo_e2e}_test.rs`, `fake_upstream/cargo.rs`,
  README "Cargo" (`:177-193`). Commits: `feat(cargo): CargoUpstream and proxy reads`, `feat(cargo): group index and
  download`, `test(cargo): tap, fake index and real cargo e2e`.
- Agent GO owns `src/registry/go/**`, `tests/{go,go_proxy,go_e2e}_test.rs`, `fake_upstream/go.rs`, README "Go modules"
  (`:206-216`). Commits: `fix(go): RFC 3339 Time, semver @latest, 404 for an unknown module list`, `feat(go):
  GoUpstream, escaped paths, proxy/group reads`, `test(go): tap and real go e2e`.
- Agent OCI owns `src/registry/oci/**` (fills `routing.rs`, adds `leaves.rs`, `upstream.rs`, splits `mod.rs` into
  `blobs/manifests/tags/uploads`), `tests/{oci,docker_e2e,oci_nested,oci_proxy,docker_cli_e2e}_test.rs`,
  `tests/common/fake_upstream/oci.rs`, `docs/api.md` OCI section, README "Docker / OCI" section (`:195-204`). Commits:
  `feat(oci): nested image names on every /v2 route` (routing only, lands first), `feat(oci): OciUpstream with bearer
  challenge and streamed blobs`, `feat(oci): group manifests, blobs and tags`, `test(oci): fake registry, docker e2e`.
- Agent OSV owns `src/telemetry/vulns/{osv,severity,deps}.rs` and the bodies behind the `vulns/mod.rs` signatures fixed
  in commit 4 (section 9), `src/api/vulns.rs`, `tests/vuln_test.rs` (including its `OPENCARGO_NETWORK_TESTS` gate),
  `tests/common/fake_osv.rs` (stub from commit 5), `Cargo.toml`/`Cargo.lock` (`cvss` only), README vulnerability paragraph (`:158-160`),
  `frontend/src/core/types.ts` (`VulnEntry`/`VulnReport`), `api.ts` (`fetchVulns`/`rescanVulns`), the new `frontend/src/core/vulns.ts` +
  `vulns.test.ts`, `frontend/src/pages/PackageDetail.tsx` (`:27-33` and `:465` only); never a publisher (`registry/publish.rs` is frozen).
  Commits: `feat(vulns): per-advisory severity from OSV records and CVSS`, `test(vulns): deterministic fake OSV`, `fix(frontend): map the vulns
  response, severity chip without CVSS` (section 9).
- Frozen during the parallel phase: `src/{server,error,config,main}.rs`, `src/server/rewrite.rs`, `src/auth/**`, `src/proxy/**`,
  `src/registry/{mod,resolve,publish}.rs`, `src/db/**`, `src/storage/**`, `src/api/repositories.rs`,
  `src/telemetry/cleanup.rs`, `tests/common/{mod,upstream_tap}.rs`, `tests/common/fake_upstream/mod.rs`,
  `tests/{group_resolver,permissions,features}_test.rs`, `Makefile`, `ci.yml`. A needed foundation change is a green
  commit on the base, rebased onto.

Integration (foundation agent): merge the five branches (README hunks in distinct sections; one `fake_upstream/*.rs`
file per agent; Agent OSV's `Cargo.lock` hunk is `cvss` only, on top of commit 2's), full suite after each merge, then `ci: go toolchain,
OPENCARGO_E2E_REQUIRE, Makefile targets` (those two files only) and `docs: proxy/group for every format, nested OCI
names, OSV severity, CHANGELOG` (drops `README.md:148-153`'s npm-only and single-segment statements, adds the sumdb,
credentials, `token_realms`, `dl_allow_private`, `[cleanup]` and `fail_closed` notes).

## 13. Risks

- Group error policy moves from a blanket 404 to 502 on upstream failure (commit 4b): right for go/cargo/docker, a
  visible change for npm clients (CHANGELOG; softened by stale-on-error and negative caching). Nested-name routing
  relies on axum percent-decoding `%2F` in a path param; `six_route_shapes` pins it.
- Cargo `dl` templates and Bearer `realm`s are upstream-controlled URLs; `validate_upstream_url`, `is_blocked_ip` (unless
  `dl_allow_private`) and the redirect policy apply, credentials never leave the upstream host or `token_realms`; DNS
  rebinding stays unmitigated as today.
- OCI's post-token 401/403 -> `Miss` also negative-caches a *wrong* pull credential for `negative_secs` (warn-logged; purge clears it).
- Real-client e2e needs cargo/go/docker on the runner; local runs skip, CI fails on absence. A cold streamed blob is
  written fully before the first byte is served (only `read_idle` bounds it); the tee is a follow-up.
- Singleflight, token and advisory caches are per-process; multi-replica deployments duplicate downloads, and a waiter
  past `singleflight_wait` duplicates one in-process by design. `collect` fan-out is sequential. `MAX_BODY_BYTES`
  (1 GiB, `src/server.rs:36`) still bounds pushes: a proxied 3 GiB layer pulls but cannot be re-pushed.

## 14. As built (foundation commits 1 to 5)

Deviations reported by the implementers and accepted by review. The five parallel agents build on this state, not on the letter of sections 2 to 12 where they differ.

### Commit 1 (`955ac3f`)
- Route factories are reached as `registry::<fmt>::routes::routes()` (design writes `npm/cargo/go/oci::routes()`): the design puts `pub fn routes()` inside `routes.rs`, so the module path carries the extra segment; no re-export was added to avoid a module/function name clash.
- `finalize_publish` already takes `format: Format` in this commit (design fixes that signature in commit 4 when it moves to registry/publish.rs): it was the only way to replace the `"npm"`/`"crates.io"`/`"Go"`/`"oci"` literals with `Format::osv_ecosystem()` as the entry asks. Side effect: the real-time `package.published` event's `format` field is now the format name (`cargo`, `go`) instead of the OSV ecosystem (`crates.io`, `Go`); no test asserted the old value.
- `FromStr` for RepoKind/Format yields `AppError::BadRequest` with today's API messages (`invalid repository type: ..`); `Repository::kind()/fmt()` map that to `Internal` (design decision 1) so the API keeps its 400s and a corrupt column is a 500.
- config.json also emits `"auth-required": true` for private repositories (section 4 handler shape, not repeated in the section 12 entry): the tokenless gate is pointless without it since cargo would never send the token.
- `ensure_kind_supported` (db/kinds.rs) centralises the transitional npm-only proxy/group rule that the API and the seed each carried as raw string compares; commit 5's validate_spec replaces it. `Format::supports_kind` is implemented as specified and is its first clause.
- OCI `upload_chunk`/`complete_upload` raw `repo_type != "hosted"` checks became `ensure_hosted` + `ensure_format(Oci)`, so their 400 message is now `can only publish to hosted repositories` (was `can only push ...`); cargo yank/unyank likewise lose their bespoke `can only yank/unyank` wording. No test asserted those strings.
- `api/dashboard.rs` and `api/me.rs` still read `repo_type`/`format` as strings: they only serialise the columns, never compare them, and are outside the entry's list.
- `oci/paths.rs` helpers take the full `sha256:` digest and strip the prefix themselves (the design writes `manifest_path(image_name, name, hex)`); every call site held the full digest, so this removed seven copies of the strip. Byte-identical paths for single-segment names.

### Commit 2 (`aa94bdc`)
- remove_stale_parts: the design says only '`*.part-*` by mtime'. The implementation additionally treats a part that disappears between the directory listing and the metadata/remove call as not-ours (skipped, not counted) instead of failing the sweep with NotFound; that race is inherent since PartFile::commit renames parts into place while the sweep runs. Small private helper `vanished_is_fine` in filesystem.rs.
- read_stream maps a missing file to AppError::NotFound (mirroring `get`) rather than a raw Io error; the design gives only the signature.
- tokio-util (io) and semver are direct dependencies from this commit as the design asks, but nothing in the tree uses them yet (ReaderStream arrives with ProxyEngine in commit 3, semver with go @latest); rustc does not warn on unused crate deps, so the gate is unaffected.
- src/storage/filesystem.rs is 433 lines after this commit (was 348, mostly tests). It is an existing file, not a new one, so the ~400-line rule for new files was read as not applying; splitting it was out of scope.
- Formatting: proxy_cache.rs is rustfmt-clean; in the touched existing files only my own hunks were laid out per rustfmt by hand. Two pre-existing non-fmt hunks in filesystem.rs (safe_path, lines 41 and 57) were left as they were, per the no-cargo-fmt rule.

### Commit 3 (`904cf7d`)
- src/registry/resolve.rs is listed under commit 4 in the design, but the engine signatures of commit 3 (ProxyEngine::fetch/head, UpstreamStrategy::upstream_url, acquire_token) take `Upstream` and `CacheRepo` and return `Outcome`, so this commit creates resolve.rs with only those three types plus `Upstream::for_member` and `MAX_GROUP_DEPTH` (needed by purge's depth cap). `UrlRepo`, `Cx`, `FailurePolicy`, `Leaf`, `Collected`, `first_hit`, `collect` and `walk` are left to commit 4, as no code in commit 3 uses them (adding them would be dead code).
- TokenCache is keyed by (upstream base URL, scope) rather than the design's (realm, scope): the realm is only learned from a 401 challenge, and `send_with_auth` must attach a cached token proactively (the design's 'attaches a cached token for (realm, scope)') so a warm token skips the challenge round trip entirely; a per-upstream realm is the invariant in practice, and the struct still carries the single `inner: Mutex<HashMap<(String, String), (String, Instant)>>` the design names.
- engine.rs is a directory module (src/proxy/engine/{mod,transfer,payload}.rs plus cfg(test) fixture.rs/tests.rs) instead of one file, to stay under the ~400-line file limit; every public path (`proxy::engine::{ProxyEngine, Cached, Payload, Src, PartFile, cache_path, Timeouts, TtlConfig}`) and the test module path `proxy::engine::tests::*` match the design.
- `Payload::into_response` is the private serving helper; `ProxyEngine::stream_response` keeps the design's `&Payload` signature. `Payload` gains no clone; `Src::Bytes` is cloned (refcounted) when served.
- `forward_head` fills `Payload.digest` from `expected_sha256(a)` rather than parsing an upstream header, because engine.rs must name no format (Docker-Content-Digest is OCI's); the size and content type come from the HEAD response headers as designed.
- `ProxyEngine` stores `timeouts` as a field (needed at fetch time for `buffered_total` and `singleflight_wait`); the design's struct listing omits it while its `new()` takes it.
- The Hub default `token_realms = ["https://auth.docker.io/token"]` is applied in `Upstream::for_member` (via `proxy::auth::default_token_realms`) when the configured list is empty and the upstream host is a Hub alias, so API-created repositories get it too, not only configured ones.
- `validate_upstream_url` hardening (link-local, 0.0.0.0) is not in this commit: the design lists it under section 10 beside `validate_spec`, which is commit 5.
- The env scan covers repositories declared in config (the design's 'two configured names mangling alike'); an OPENCARGO_UPSTREAM_AUTH_*/OPENCARGO_DL_ALLOW_PRIVATE_* variable naming no configured repository is warn-logged and ignored.
- The ProxyEngine `fetch` deviates from 'engine.rs names no kind' in no way, but note that when a refresh returns a Miss on a key that previously had a body, the old file is left for the sweep (row is replaced by the negative row in place); the design does not specify this and a delete was not added.
- `AppState` keeps `proxy_client` and `proxy_default_ttl_secs` beside the new `proxy` and `upstream_auth`, since the design says ProxyClient survives until commit 4 and npm still calls it.

### Commit 3b (`45a20b6`)
- Section 11 says the npm seeding helpers (build_tarball, build_npm_publish_body) land in tests/common/mod.rs in commit 5. They land now, with the design's names, because npm_proxy_test needs them and the alternative was a third private copy (npm_test.rs and features_test.rs already duplicate them). Commit 5 still owns rewiring the existing six files onto them; nothing existing was touched.
- The harness spawns its SQLite as tmp/opencargo.db (design: expire_entries opens tmp/opencargo.db), whereas every existing per-file setup() uses tmp/test.db. No conflict, since the existing files are rewired only in commit 5.
- Tap gained one convenience method, Tap::count(path) -> usize, on top of the design's three fields (base_url, hits, fail); it is the only way the four cases read hits and avoids repeating the lock/filter in each test.
- SpawnOpts has a manual Default with anonymous_read = true (the design lists the fields but no default); every existing test spawns with anonymous_read = true, and the cargo bootstrap case will opt into false explicitly.

### Commit 4 (`8b50f96`)
- Added src/registry/npm/dist_tags.rs (GET/PUT/DELETE dist-tags handlers) beyond the design's npm file list: keeping them in publish.rs pushed it to ~500 lines. Design section 2 lists only {mod,routes,read,publish,leaves,upstream,packument}.rs.
- PackumentLeaf { name, abbreviated } does the abbreviated stripping itself in both arms (hosted arm = the moved mod.rs:397-459 logic, proxy arm strips after bytes()); the handler only rewrites tarball URLs with cx.url. Design section 4 has the handler strip; the leaf struct matches the design and no field is dead.
- PackumentLeaf::Out is a small `Packument { json, stale }` rather than a bare Value so the handler can add `Warning: 110` when the engine served a stale row (upstream_503_serves_stale_metadata asserts the header). The design leaves the JSON leaf's Out type unspecified.
- npm publish is restructured so `plan_versions` validates, decodes, checksums and runs publish_gate for every version before `create_package`/README/tarball/version writes: the design's 'gate before the first write' would otherwise leave a `packages` row (and a 200 empty packument) behind a blocked publish. Consequence: a multi-version body is all-or-nothing on validation (npm sends one version).
- publish_gate returns PreScan(None) when vuln_scan.enabled is false, preserving today's behaviour where a disabled scanner records nothing; scan_version likewise persists only when enabled. Design section 9 does not spell out the disabled case.
- ScanError is defined as { Upstream(String), Db(sqlx::Error) } (only ::Upstream is named in the design). api/vulns.rs rescan now maps it to 503 ServiceUnavailable instead of 500 Internal (its error type had to change; section 10 assigns OSV faults to ServiceUnavailable). Agent OSV owns that file.
- VulnScanConfig gaining three fields broke exhaustive struct literals in tests/{vuln,authz,ws}_test.rs; each got `..Default::default()` (4 one-line edits). Not listed for commit 4 but required for the gate.
- vuln_test::default_config_has_osv_url_and_concurrency was added here (section 9 names it as pinning the manual Default that this commit introduces); the live osv.dev gate for that file stays with Agent OSV as planned.
- npm read handlers now call validate_package_name("npm") and a new validate_tarball_filename (400) before building any key or URL, per the common handler shape of section 4; today's reads validated nothing.
- A lone proxy repository whose upstream_url is missing/invalid now yields 404 via the resolver's failure path (Upstream::for_member error recorded as a member failure under FailurePolicy::NotFound) instead of today's 500; 4b turns it into 502.
- rewrite_tarball_urls stays in src/proxy/mod.rs as the design references it there, although the proxy layer is otherwise format-neutral and proxy/** freezes during the parallel phase.
- search and search_in_repo were moved verbatim into npm/read.rs and exceed 80 lines (81/83); untouched per 'search keeps today's semantics' and owned by Agent NPM next. Other >80-line functions flagged (cargo/go/oci publish, build_state, build_router, main) pre-exist and only received the gate insertion.
- src/api/repositories.rs purge_cache still runs its raw `DELETE FROM proxy_cache_meta` (commit 5 wires purge_repository); it no longer clears proxy_cache_entries rows, but the engine treats a row whose file was deleted as a miss, so purge still forces a refetch.

### Commit 4b (`3a81466`)
- Design 12.4b says to 'update' proxy_test::test_group_repo_falls_through_to_proxy / test_proxy_handles_nonexistent_package (ba21753 lines 418/455). Their code and assertions were already correct under the new policy (hosted miss then proxy hit -> 200; npmjs 404 -> negative-cached 404), so the update is limited to their doc comments, which stated the old 'any 404-level error -> 404' contract; they now spell out 404/410 -> 404 vs anything else -> 502 and point at the offline npm_proxy_test case. No assertion was weakened or changed.
- Line references in the design (proxy_test.rs:418, 455) are stale against the current tree (now :438 and :478, because commit 4 added the OPENCARGO_NETWORK_TESTS gate); followed the code.
- src/registry/resolve.rs was already not rustfmt-clean at HEAD (7 hunks), so per the 'match surrounding style, no fmt on files you did not change' rule the rewritten test lines keep the file's existing one-line assert!(matches!(..)) shape rather than being reformatted; the new npm_proxy_test case mirrors group_falls_through_to_proxy verbatim for the group(..) literal for the same reason.

### Commit 5 (`0e5734b`)
- Delete status: the design's group_resolver cases say 'expects 204', but delete_repository has always answered 200 with {"ok": true} and permissions_test::test_delete_repository pins that; kept 200 (code reality) and asserted 200 in the new tests.
- validate_upstream_url hardening (link-local 169.254/16 + fe80::/10 and 0.0.0.0/:: refused, loopback and RFC 1918 still allowed) is listed in sections 2 and 10 but assigned to no commit; since src/proxy/** is frozen after commit 5 and validate_spec routes every upstream through it, it landed here. Covered by create_update_seed_validate_members_and_upstream; the design's own token_realm_on_link_local_is_refused stays with Agent OCI.
- Cargo seeding helpers (build_cargo_publish_body, build_crate_data) went to tests/common/mod.rs rather than only tests/cargo_test.rs: features_test::test_dashboard_hides_private_packages (a dashboard test, stays in the frozen features_test) publishes a crate, so the helpers must be shared to avoid a second copy.
- Shared helpers were generalised where the originals hard-coded a repo: push_blob takes the image path ('oci-private/myapp'), publish_go_module takes the repo name; call sites updated in oci_test/go_test.
- resolve::error_policy_table: its fixture seeded deliberately invalid groups (unknown member, cross-format member, empty list, a cycle) through build_state, which validate_spec now refuses at seed time; those rows are now written by raw SQL after build_state, as pre-upgrade rows would be. Every assertion is unchanged.
- go_test::test_go_publish_rejects_invalid_module_names: the slash-in-version request used `v1.0%2F0`, which only reached the handler because the old go_test setup lacked main.rs's decode_percent_encoded_slashes layer; with the harness (which the design says ALWAYS applies it) `%2F` becomes a path separator and the request never hits the publisher. Changed the fixture to `%252F` so it reaches the handler as a version containing a slash; the 400 assertion is unchanged.
- tests/common/mod.rs is 446 lines and tests/group_resolver_test.rs 671 lines after rustfmt, above the ~400 guideline: the design names both files as the single home of, respectively, all shared helpers and all ten resolver/admin cases.
- validate_spec detects group cycles by walking members already in the DB with a seen-set (no MAX_GROUP_DEPTH import into db/); a cycle between two entries of one config file is caught when the second is validated (the first is inserted by then). Nesting depth is not checked at write time (not in the design); the read-time Internal('group nesting depth exceeded') is pinned instead.
- permissions_test.rs was touched only for the section 11 reorder/flip (its local helpers were not rewired; the design names six other files for the rewire). The old test name no longer described the matrix, so it is test_create_repository_validates_kind_format_and_members.
