# Policy report: "would have blocked"

Design contract for the launch demo feature: every artifact served through a proxy member with at least one policy rule enabled is recorded as a resolution event, the enabled
rules are evaluated against it, and an admin report shows what each rule *would* have blocked. Nothing is ever blocked: rules never change a response. Line references are to
`main @ 393242a`; re-grep before editing. Builds on `multi-format-proxy.md` (resolver, engine, OSV, harness), unchanged.

## 1. Decisions

1. One event per served artifact: npm tarball, cargo `.crate`, go `.zip`, OCI manifest or index GET. Packuments, index lines, `config.json`, `.info`/`.mod`, blobs, tags lists
   and manifest HEADs are inputs and record nothing. A multi-arch `docker pull` is one event: an index GET is parked in the writer until one GET of a child digest arrives within 60 s, counted per key, each count stamped
   and dead 60 s after its index whatever the writer did in between, so N concurrent pulls of one image suppress exactly N children and record N rows, each dated from the child actually served (section 4). A hosted arm never records; a group whose hosted member answers first records nothing.
   Only OCI guards on the method (`!self.head`: `docker` HEADs every manifest before its GET); axum's `get()` also answers HEAD (`npm/routes.rs:15,17`, `cargo/routes.rs:19`, `go/routes.rs:15`) and `Cx`
   carries no method (`resolve.rs:25-29`), so a HEAD on a tarball, `.crate` or `.zip` records like a GET: no npm, cargo or go client sends one, an accepted asymmetry rather than a `Cx` change.
2. Recording is one call in each of the four proxy leaf arms (`policy::record`), not an engine hook (section 3). The leaf hands over identity plus a `Source` handle; every fact
   read and every extra request happens in the recorder.
3. The publish date comes from the cache when it holds it (`ProxyEngine::peek`), else from a bounded number of extra requests through `ProxyEngine::fetch`, made only when an
   enabled rule needs the fact: npm one packument, or one conditional refresh of it when the cached copy predates the version (`min_release_age` or `install_scripts` on), cargo one paced
   crates.io API call, go one `.info`, OCI one config blob and never a manifest (an index is dated from the child the client pulled); `fetch_missing_facts = false` (default true)
   turns them all off. Dates parse as RFC 3339, then as SQLite's `%Y-%m-%d %H:%M:%S` as UTC. A missing date is `unknown`, never `would_block`; so is an OCI `created` before 2000-01-01 (section 5.1).
4. Rules are strategies behind one `Rule` trait, configured per proxy repository in `[policy.<repo>]`, all off by default. Recording is gated on the same config: a member with
   no rule enabled writes nothing (a borrow-only check before anything is cloned), so an upgrade records nobody's downloads until an admin turns a rule on. One writer task
   receives continuously from a bounded channel and runs gather + the pure rules concurrently under a semaphore, so a slow upstream stalls one slot, never the queue, and
   parks on the channel when idle (section 3); crates.io version lookups are paced at one request per second per host (section 4); `osv_severity` is answered per flush, one
   `querybatch` per ecosystem for up to 64 rows, under the scanner's own `max_concurrency`; the page learns of new rows through one coalesced `policy.resolution` per flush,
   at most two a second per `(requested_repo, member_repo)` pair, never one per row (section 3). A full channel drops and counts the event, never delays the response. A rule failure yields `unknown`, never a client error.
5. Rows live in two tables (migration 014): `policy_resolutions` and `policy_verdicts`, purged by the existing cleanup sweep after `cleanup.policy_report_older_than_days`
   (default 90), which also starts the sweep task, and erasable per user through one admin route whose audit entry carries a count, never the name (section 7).
6. Every row carries a display label `actor` (the API token's name if any, else the username, `static-token` for config tokens, else `anonymous`) and the identity `user_id`
   (`users.id`, the token's owner for a token). Token names are free text and not unique (`003_auth.sql:11-21`; `create_token` takes `body.name` verbatim, `tokens.rs:62-118`),
   so `/me/policy` and `DELETE` work on `user_id`, never the label. `AuthUser` gains `token_name: Option<String>`; it already has `user_id`. Client IP and User-Agent stay out of
   scope. The label is personal data: `README.md` says what is recorded, for how long, how erased; startup warns which members record; `/api/v1/me/policy` shows one's own rows.
7. No new crate: the one-edit classifier is ~40 lines in-crate, top-N lists are `include_str!`, the writer uses tokio `mpsc`/`Semaphore`/`JoinSet`/`interval` (`Cargo.toml:15`).
8. `typosquat` never strips an npm scope, passes any name on a 20 k known-name list before comparing (`mssql` is not a squat of `mysql`), flags one substitution, one transposition or one
   separator **dropped** from a top-5 k name over names of 5+ characters, never a letter added or removed (`requests`, `colours`) nor a separator added (`is-array` next to `isarray` is
   npm's oldest convention; every cited squat drops one), treats a Go major suffix as the same module, and is `not_applicable` for OCI: a misspelt official image 404s on Docker Hub (section 5.4).
9. Durations in policy config (`min_release_age`) and in the report filter (`since`) share one parser, `Age`, accepting `s`/`m`/`h`/`d` and nothing else; an unparsable value
   refuses startup (config) or answers 400 (API). The proxy's `parse_duration_secs` (`src/server.rs:674-685`) is not reused: no `d`, and it silently yields 10 s on any error.

## 2. Module layout

```
src/
  policy/mod.rs        PolicyEngine, record(), Pending, Source, Resolution, Actor, Verdict, Facts;  policy/age.rs  Age(Duration): FromStr/Deserialize/Display for "30s" "10m" "48h" "7d";  lib.rs  + pub mod policy
  policy/writer.rs     run_writer(): recv loop, inline OCI classification (parked indexes), semaphore-bounded JoinSet, spawned flush (OSV batch + insert + coalesced WS event);  policy/pacer.rs  Pacer (crates.io)
  policy/facts.rs      per-format fact gathering (date, version, install scripts, digest, index children); npm PackageFacts memo (one parse per packument row)
  policy/store.rs      insert_batch, list_resolutions, report_totals, delete_older_than, delete_by_user (sqlx only);  policy/startup.rs  startup_notes(&Config), pure
  policy/rules/mod.rs  Rule trait, PolicyConfig (+ is_empty, needs_packument, hand-written Default), all_rules(), evaluate_all(); rules/{min_release_age,osv_severity,install_scripts,typosquat}.rs
  policy/lists/{npm,crates,go}.txt + known/{npm,crates,go}.txt + NOTICE (section 5.4);  policy/distance.rs  one_edit(a, b) -> Option<Edit>, normalize(format, name)
  api/policy.rs        GET /api/v1/policy/report, DELETE /api/v1/policy/report?user=|user_id=, GET /api/v1/policy/rules, GET /api/v1/me/policy
  db/migrations/014_policy.sql;  db/mod.rs  Repository derives Clone;  config.rs  Config.policy, CleanupConfig.policy_report_older_than_days;  auth/middleware.rs  AuthUser.token_name, live_api_token_by_id
  proxy/engine/mod.rs  + peek(), refresh();  proxy/engine/payload.rs  Cached derives Clone
  telemetry/metrics.rs + policy_dropped_total;  telemetry/vulns/{mod,osv}.rs  assess_batch, permit in query_batch;  telemetry/cleanup.rs  policy sweep, start guard += policy bound
  registry/{npm,cargo,go,oci}/leaves.rs   one `policy::record(..)` line each over the `Found(cached)` before `into_payload` (oci: + fetch_cached);  cargo/upstream.rs + VersionMeta
frontend/src/core/{api.ts,types.ts,stores/policy.ts,stores/policy.test.ts}  pages/admin/PolicyReport.tsx
tests/policy_test.rs;  tests/common/mod.rs (SpawnOpts.policy/.policy_tuning, named_token, wait_for_policy_rows, sentinel);  fake_upstream/{npm,oci,cargo}.rs (tarballs, indexes, version API + 429, latency)
```

## 3. Event model and the recording point

```rust
// src/policy/mod.rs
/// `name` is display only, `user_id` the identity. Actor::of(Option<&AuthUser>): Token if `token_name`, User if `user_id`, Static for the config-token admin (`static_token_user`, `middleware.rs:387`, `user_id: None`), else Anonymous
#[derive(Debug, Clone, Serialize)] pub enum ActorKind { Token, User, Static, Anonymous }   pub struct Actor { pub name: String, pub kind: ActorKind, pub user_id: Option<i64> }
/// Values copied from the served `Cached` (never the handle); Oci holds the served manifest or index row, `served` the child row the writer attaches to a released index (section 4).
#[derive(Debug)] pub enum Source { Npm { filename: String, digest: Option<String> }, Cargo { cksum: String }, Go { digest: Option<String> }, Oci { body: Cached, served: Option<Cached> } }
/// `name` is the client-facing name (the row's `name`, sections 6 and 8: `nginx`, never `library/nginx`); the upstream's own name is re-derived by `facts::oci_blob` for its one request and never stored (section 4).
#[derive(Debug)] pub struct Pending { pub requested_repo: String, pub member: Repository, pub upstream: Upstream, pub format: Format, pub name: String, pub version: Option<String>, pub actor: Actor, pub source: Source }
#[derive(Debug, Clone)] pub struct Resolution { pub requested_repo: String, pub member_repo: String, pub format: Format, pub name: String, pub version: Option<String>, pub digest: Option<String>, pub actor: Actor, pub published_at: Option<DateTime<Utc>>, pub facts: Facts }
#[derive(Debug, Clone, Default)] pub struct Facts { pub install_scripts: Option<bool>, pub date_source: &'static str }   #[serde(rename_all = "snake_case")] pub enum Verdict { Pass, WouldBlock, Unknown, NotApplicable }   pub struct RuleVerdict { pub rule: &'static str, pub verdict: Verdict, pub reason: String }
#[derive(Clone)] pub struct PolicyEngine { tx: mpsc::Sender<Pending>, shared: Arc<Shared> }
struct Shared { db: SqlitePool, proxy: ProxyEngine, rules: Vec<Box<dyn Rule>>, config: HashMap<String, PolicyConfig>, events: Arc<EventBus>, scanner: Arc<VulnScanner>,
    recent_children: Mutex<HashMap<(i64, String, String), VecDeque<(u64, Instant)>>>, /* child key -> (parked index seq, parked at); expiry decided at pop, section 4 */ parked: Mutex<HashMap<u64, (Pending, Instant)>>,
    npm_facts: Mutex<Memo<(i64, String), NpmSlot>> /* section 4: one parse per row, coalesced */, osv_memo: Mutex<Memo<(String, String, String), OsvFinding>> /* section 5.2 */, notify: Mutex<Notify>, inflight: Arc<Semaphore>, cargo_pacer: Pacer, tuning: Tuning, dropped: AtomicU64 }
#[derive(Clone, Copy)] pub struct Tuning { pub child_ttl: Duration, pub pacer_period: Duration, pub pacer_cooldown: Duration, pub gather_timeout: Duration, pub notify_period: Duration, pub refresh_floor: Duration }   // 60 s, 1 s, 60 s, 15 s, 500 ms, 5 min = `Tuning::default()`; tests shrink them through `new_tuned` (harness) or `unspawned` (in-crate)
impl Tuning { pub fn pacer_waiters(&self) -> usize; }   // `(gather_timeout / pacer_period).max(1)`: 15 at defaults; the pacer's waiting-line cap (section 4)
impl PolicyEngine {
    /// Builds the channel (capacity QUEUE = 4096) and `tokio::spawn`s the writer; called from `build_state`, always under a runtime.
    pub fn new(db, config, scanner: Arc<VulnScanner>, events, proxy: ProxyEngine) -> Self;   // = `new_tuned(.., Tuning::default())`
    #[doc(hidden)] pub fn new_tuned(db, config, scanner, events, proxy, tuning: Tuning) -> Self;   // spawning; the harness's only way to a tuning knob (below)
    pub(crate) fn unspawned(.., tuning: Tuning) -> (Self, impl Future<Output = ()>);   // writer future returned, not spawned: the `writer::*` unit tests drive it
    pub fn records(&self, member: &str) -> bool;   pub fn config_for(&self, member: &str) -> PolicyConfig;   // `config.get(member).is_some_and(|c| !c.is_empty())`, a borrow, the leaves' gate; clone or `PolicyConfig::default()` (rules off, fetch_missing_facts = true), writer only
    pub fn record(&self, p: Pending);   pub async fn record_now(&self, p: Pending) -> AppResult<Option<i64>>;   // `try_send`: `Full` -> `dropped += 1`, metric, one `warn` per minute, never awaits; the inline variant (classify + gather + evaluate + insert + emit, None when suppressed) is for tests
}
/// The one line each leaf adds. Returns at once unless `records(member.0.name)`; only then clones member/upstream, calls `source`, builds `Pending`.
pub fn record(cx: &Cx<'_>, member: CacheRepo<'_>, up: &Upstream, format: Format, name: &str, version: Option<String>, source: impl FnOnce() -> Source);
```

`AppState` (`src/server.rs:41-58`) gains `pub policy: PolicyEngine`, built in `build_state` after `vuln_scanner` (`:179`), `events` and `proxy`. `PolicyEngine::new` spawns the writer itself, so
`build_state` keeps its `async fn (&Config) -> anyhow::Result<AppState>` signature (`server.rs:60`) and its fourteen callers, all under a runtime, are untouched (`main.rs:76`, `resolve.rs:325`,
`tests/common/mod.rs:106,141`, ten `tests/*_test.rs`); it also logs one `warn` listing the members whose config records (decision 6). `tests/common/mod.rs` is its own crate and sees only `pub`
items (`use opencargo::server`, `:24`), so `spawn_in` applies `SpawnOpts.policy_tuning` by replacing `state.policy` with `PolicyEngine::new_tuned(state.db.clone(), &config, state.vuln_scanner.clone(),
state.events.clone(), state.proxy.clone(), t)` between `build_state` and `build_router` (`:106-109`; every `AppState` field is `pub`); the first engine's writer sees `recv() == None` once its last
`Sender` is dropped and returns, so the loop's `None` arm is `break`. `Repository` (`src/db/mod.rs:15-26`) derives `Clone` so `Pending` owns its member; `Upstream` already does (`resolve.rs:37-43`);
both clones and the OCI `Cached` clone happen only after `records` said yes, so on today's deployments (no `[policy.*]`) a served artifact costs one `HashMap` lookup and, OCI included, zero added
queries: the auth middleware learns `token_name` from rows it already reads (section 7). `record` is synchronous and allocation-only: the leaf's `Found` path never waits on storage, OSV or SQLite.
A `Pending` is a few strings plus, for OCI, one `CacheEntry` (`proxy_cache.rs:4-16`), under 1 KB: `QUEUE = 4096` is a few MB. The leaf shape, npm (`npm/leaves.rs:96`) and go (`:172`) alike:
`let cached = engine.fetch(..).await?; if let Outcome::Found(c) = &cached { policy::record(.., || Source::Npm { .. digest: c.entry.digest.clone() }) } Ok(cached.into_payload())`;
cargo passes `cksum`; oci goes through a new `fetch_cached` (section 4) and clones the `Cached`.

`src/policy/writer.rs`: `run_writer(rx, shared)` is a `select!` loop over four branches, never a drain-then-work cycle. (1) `rx.recv()`: an OCI event first goes through `facts::oci_classify` **inline, in
arrival order** (a local file read of the served body, no network; section 4): an index is parked, a child that releases a parked index becomes that index's event carrying the child's row, a suppressed
child ends there; everything else takes one `inflight.acquire_owned()` permit (`INFLIGHT = 64`) and is spawned into a `JoinSet` running `facts::gather` + `evaluate_all`. The task has no deadline of its
own: `facts` wraps each extra request in `timeout(tuning.gather_timeout, ..)` (15 s, section 4); a timed-out request yields `None`, `date_source = "timeout"`, and the row is still written. The permit dies
with the task, so one hung upstream costs one slot for at most 2 x `gather_timeout` = 30 s, the cargo lane wait being under the same deadline (one request per event for every format; the engine's `buffered_total = 60 s`, `engine/mod.rs:40-46`, is never
reached) while the loop keeps receiving. (2) `Some(done) = tasks.join_next(), if !tasks.is_empty()`: a finished `(Resolution, Vec<Option<RuleVerdict>>)` joins `ready`, flushed at `BATCH = 64`. (3) `_ =
tick.tick(), if !ready.is_empty() || !parked.is_empty() || !recent.is_empty() || notify.pending()`, `tick = interval(100 ms)`: flushes `ready`, releases indexes parked past `child_ttl`, sweeps `recent_children` entries past it, sends a due coalesced event. (4) `Some(_)
= flushes.join_next(), if !flushes.is_empty()`: reaps a finished flush. The preconditions are load-bearing: `JoinSet::join_next` is `Ready(None)` on an empty set, so without them the loop spins at 100 %
CPU whenever no task runs, the steady state of an instance with a rule on and no traffic; with them an idle writer holds no timer and is woken by `rx` alone (`idle_writer_is_not_repolled`, section 9); `recent_children` is in the guard so its sweep runs until the last entry dies, `child_ttl` after the last index pull, then the timer stops too. The sweep is hygiene, not correctness: a key's expiry is read from the entry at pop time (section 4).
`flush` is **spawned** into `flushes`, so the receive loop never waits on OSV or SQLite: `osv_severity::evaluate_batch` (section 5.2), one `store::insert_batch` transaction, then `notify.offer(batch)`:
**one** `policy.resolution` per `(requested_repo, member_repo)` pair per flush, `{repo, member, count, would_block, unknown}`, sent at once when `notify_period` (500 ms) has passed since the last, else
folded into `pending` for the next flush or the tick. Never one per row: `EventBus` is a single `broadcast::channel(256)` for every event type and subscriber (`src/events.rs:44-59`) and the WS loop sends
inline (`api/ws.rs:94`), so per-row events at the rates below would lag every browser into `resync` (`ws.ts:113`), a refetch of every live store, and evict `package.published` and `audit.entry` for
everyone; the page debounces at 300 ms with a 2 s max-wait and loses nothing (section 8). A batch that fails to insert is logged at `error` and dropped: best-effort telemetry. Flushes may overlap; row order across them is
irrelevant. Throughput is `INFLIGHT / gather latency` for cold facts (64 slots at 200 ms = 320 cold events/s; warm facts are a memo hit or one small local read, thousands/s, except the first npm event per
packument row, which parses it once, section 4; cargo cold dates are paced apart, section 4) and one SQLite transaction per 64 rows; the channel absorbs bursts above the cold rate (500/s for 10 s backs up
~1 800 events, under `QUEUE`) and fills only when clients sustain more. Helpers `next_event`, `spawn_work`, `flush` keep every function under 80 lines; `record_now` runs the same steps inline for tests.

Why the leaf, not the engine or the resolver. `ProxyEngine::fetch` (`src/proxy/engine/mod.rs:92-163`) knows the member and `S::Artifact` but neither `cx.auth` nor which artifact is "the
served one" (packument, index line, `.info`, config blob, every layer); telling them apart there would name formats. `walk` (`resolve.rs:174-180`) knows no name or version. The leaf `proxy`
arm has `cx.auth`, `member`, `up` and the identity in scope (`npm/leaves.rs:85-97`, `cargo/leaves.rs:81-103`, `go/leaves.rs:156-173`, `oci/leaves.rs:119-145`); four one-line calls guarded by
`Outcome::Found` beat a trait method plus a resolver change, and the untouched `hosted` arms are the proof.

`ProxyEngine::peek<S>(&self, s: &S, member, a: &S::Artifact) -> AppResult<Option<Cached>>` (`src/proxy/engine/mod.rs`, next to `head`): a cached body if one exists, fresh or stale, without
any upstream request or row touch: `get_entry` (`src/db/proxy_cache.rs:38-56`) + `resolve_row` (`engine/mod.rs:252-270`) on a `status 200` row, then `Cached { entry, stale: !fresh }`; stale
is fine for a row keyed per version (`.info`, `VersionMeta`, an OCI digest): a released version's facts are immutable. The npm packument is keyed per package and may predate the version,
hence `ProxyEngine::refresh<S>(..) -> AppResult<Outcome<Cached>>`: `fetch`'s lookup, singleflight and `exchange` (`engine/mod.rs:99-125`) with the fresh-hit return on a `200` row (`:115-118`; `:108-113`, the negative-row branch, kept) skipped, so a `200` row, fresh or not, becomes `Stale { row,
target }` and the request is conditional (`If-None-Match` from `row.etag`, `engine/transfer.rs:45-46`); a 304 touches the row and moves no body (`:127-140`; `touch_entry` leaves `fetched_at` alone, `proxy_cache.rs:91-94`, so the memo stays valid). **Read-only on failure**: `fetch`'s
`Miss` arm (`:146-148`) calls `record_miss`, which deletes the stale row's file (`:233`) and upserts a `status 404` row under `negative_secs` (`:247`, `negative_cache_ttl` = 1 h), fine for a client
that asked and got a 404, fatal for a recorder-initiated refresh: npm answers 404 for a private package whose token was rotated, and a packument served from cache a second earlier would 404 every
client for an hour, breaking the contract that rules never change a response. So `fetch` is split into `lookup` + `exchange` + `settle(reply, stale, Miss::Record)` and `refresh` is the same three with `Miss::Ignore`:
`NotModified` and `Stored` as in `fetch`, everything else (`Miss`, `Refused`, `Failed`, `Err`) is `Ok(Outcome::NotFound)` with no row touched, no file deleted, no negative row, the caller noting `not-in-packument`; `engine::tests::refresh_never_records_a_miss` (section 9) is the proof. Bodies: `engine.bytes(&Cached)` (`engine/mod.rs:194-204`) reads the file from `FilesystemStorage`, the only backend (`src/storage/`).

## 4. Per-format facts (`src/policy/facts.rs`)

`gather(shared, p: Pending) -> Resolution` never fails: every reader swallows its errors into `None`, logs at `debug` and names its outcome in `facts.date_source` (`cache`, `fetch`,
`refresh`, `not-in-packument`, `annotation`, `config-blob`, `unset-created`, `index-unpulled`, `not-fetched`, `rate-limited`, `filename-unparsed`, `timeout`, `none`). `fetch_or_peek(shared, cfg, artifact)` is `peek`, then `timeout(tuning.gather_timeout, fetch)` only when `cfg.fetch_missing_facts` (default `true`), else `None` with
`not-fetched`; `Elapsed` is `None` with `timeout`. The deadline lives here, on each request, never on the gather task (section 3).

| format | leaf line | version | digest | published_at | facts |
|---|---|---|---|---|---|
| npm | `TarballLeaf::proxy` on the `Found(cached)` (`npm/leaves.rs:96`) | `npm_version(&name, &filename, packument)`: the `{unscoped}-{version}.tgz` stem when it starts with the unscoped name; else the `versions[*].dist.tarball` whose last path segment equals `filename` (`read.rs:24-37` validates only the character set, not the prefix; hosted matches `tarball_path.ends_with`, `leaves.rs:73`, so mirrors may serve any stem); else `None` with `date_source = filename-unparsed` | `Source::Npm.digest` = `cached.entry.digest` copied in the leaf (`sha256` hex of the tarball, set by the engine) | packument only when `cfg.needs_packument()` (`min_release_age` or `install_scripts` on): `package_facts(name)` (below) -> `versions[version].published_at`; a version absent from a **peeked** packument is a miss, not a fact: `refresh` under `fetch_missing_facts`, at most once per package per `refresh_floor` (5 min), parse again, else `None` with `not-in-packument` | `install_scripts = versions[v].hasInstallScript == true` or `versions[v].scripts` has preinstall/install/postinstall; `None` when the packument was not read or the version is unresolved |
| cargo | `CrateLeaf::proxy` (`cargo/leaves.rs:101`) | `self.version` | `Source::Cargo.cksum` from the index line (`:92`) | only when `min_release_age` is on: `peek(CargoArtifact::Config)` -> `api` (the upstream's own value, `https://crates.io` for crates.io; `index.rs:54` rewrites only the copy served to clients) -> `peek(VersionMeta{api, name, version})`, else `cargo_pacer.fetch(..)` = one paced `GET {api}/api/v1/crates/{name}/{version}` (below) -> `version.created_at`; no `api` key -> `None` | default |
| go | `FileLeaf::proxy` on the `Found(cached)` when `kind == FileKind::Zip` (`go/leaves.rs:172`) | `unescape(&self.version)` (`go/escape.rs:19`) | `Source::Go.digest` = `cached.entry.digest` copied in the leaf | `peek(GoArtifact::File{kind: Info})`, else `fetch_or_peek` -> `Time` | default |
| oci | `ManifestLeaf::proxy` when `!self.head` (`oci/leaves.rs:138-144`) | `reference` (tag or digest) | `sha256:` + `body.entry.digest` (`:140`) | the manifest body: `body` for a manifest, `served` (the child row the client pulled, attached by the writer) for an index, `None` with `index-unpulled` when no child came within `child_ttl` (`docker manifest inspect`): `annotations["org.opencontainers.image.created"]`, else `fetch_or_peek(oci_blob(up, &p.name, &config.digest))` -> `.created` (a blob GET under the **upstream** name, singleflighted with the client's own config pull; never a manifest request); a `created` before 2000-01-01 is `None`, `unset-created` (section 5.1) | `index children` recorded, see below |

```rust
pub fn parse_time(s: &str) -> Option<DateTime<Utc>>;   // RFC 3339, then "%Y-%m-%d %H:%M:%S" as UTC, else None
pub fn npm_version(name: &str, filename: &str, packument: Option<&Value>) -> Option<String>;   pub fn oci_children(body: &[u8]) -> Vec<String>;   // index -> manifest digests
pub async fn oci_classify(shared, p: Pending) -> Option<Pending>;   // writer-inline, arrival-ordered: parks an index, a child releases it (Some(index) with `served`); None = parked or suppressed
pub async fn npm(shared, cfg, member: CacheRepo<'_>, up: &Upstream, name: &str, filename: &str) -> (Option<String>, Option<DateTime<Utc>>, Option<bool>, &'static str);
pub struct PackageFacts { pub row_id: i64, pub fetched_at: String, pub versions: HashMap<String, VersionFacts /* published_at, install_scripts, tarball filename */> }   pub async fn package_facts(shared, cfg, member, up, name: &str, want: &str) -> Option<Arc<PackageFacts>>;   // memo hit, or peek/refresh + one coalesced parse; `want` decides the refresh
pub struct NpmSlot { pub stamp: (i64, String) /* row id, fetched_at */, pub cell: Arc<OnceCell<Arc<PackageFacts>>>, pub refresh: Option<(Instant, Arc<OnceCell<()>>)> }   // tokio::sync::OnceCell: concurrent misses await one parse, one refresh
pub async fn {cargo,go}_published_at(shared, cfg, member, up, name: &str, version: &str) -> (Option<DateTime<Utc>>, &'static str);   // oci: `body: &[u8]` for `version`
pub fn oci_blob(up: &Upstream, name: &str, digest: &str) -> OciArtifact;   // pure: `Blob { name: upstream_name(up, name), digest }`; the one place the recorder turns the client's name into the upstream's
```
`oci_blob` exists because the two OCI names differ: `ManifestLeaf::proxy` computes `upstream_name(up, &self.name)` and moves it into the artifact (`oci/leaves.rs:125-133`; `OciArtifact.name` "is already the
upstream's name", `oci/upstream.rs:22-23`), while `Pending.name` is `self.name`, the client's. For Hub (`is_hub`, `oci/upstream.rs:62-73`) `nginx` is `library/nginx` upstream; a blob GET under the display name
goes to `/v2/nginx/blobs/..`, which Hub answers 401 -> `Classified::Refused` (`:190-196`) -> `Outcome::NotFound`, one wasted token exchange and `unknown` for every unannotated official image, under the only
live OCI rule. No harness case can see this: every fake registry listens on `127.0.0.1`, never a `DOCKER_HUB_HOSTS` name (`proxy/auth.rs:13`), so `upstream_name` is the identity there and `hub_shape`
(`fake_upstream/oci.rs:37`) only turns 404 into 401. The proof is in-crate: `facts::oci_blob_uses_upstream_name` (section 9) builds an `Upstream` on `https://registry-1.docker.io` (all fields `pub`, `resolve.rs:38-43`).
The digest is never gathered: `gather` moves it out of `p.source` (Npm/Go `digest`, Cargo `cksum`, Oci `body.entry.digest`) into `Resolution.digest`.

Index children. `from_engine` (`oci/leaves.rs:13-29`) returns `Outcome<Payload>` for HEAD and GET alike (`engine.head` yields `Payload`, `engine/mod.rs:188`) and stays as is; commit 1 adds
`fetch_cached(cx, member, up, a) -> AppResult<Outcome<Cached>>` (`engine.fetch` without `into_payload`), used by `ManifestLeaf::proxy` when `!self.head` (`:138`), which records over the `Found(cached)` and
calls `into_payload` itself; `BlobLeaf` keeps `from_engine`. `Cached` is `#[derive(Debug)]` alone today (`engine/payload.rs:18-22`) and gains `Clone`, which its one field `CacheEntry` already derives
(`proxy_cache.rs:3`). `oci_classify` runs in the writer's receive loop, one event at a time, in channel order, on the body's `mediaType` (`image.index` or `manifest.list`), not on the request shape:
`docker pull name@sha256:<index>` parks like a tag pull. An index is **parked**, `parked[seq] = (Pending, now)`, and `(seq, now)` is pushed on `recent_children[(member.id, name, child_digest)]` for every child
digest. A manifest GET whose key holds entries first drops every front entry older than `tuning.child_ttl` (60 s), **then** pops the front one and is suppressed; a key left empty is removed and the pull records on its own. Expiry is decided at pop time from the entry's own stamp, never by the sweep alone: the tick runs only while the guard keeps the writer awake, and a pop trusting an unswept
entry would swallow a real pull hours later (an amd64 fleet leaves every arm64 key holding a dead seq; a CI job's `nginx@sha256:<arm64>` must record, the last leg of `oci_index_pull_is_one_row_dated_from_served_child`). If the popped seq is still parked, the index leaves the map and is spawned as one event whose `Source::Oci.served` is the child's own `Cached`, the manifest body the leaf already handed over: no request, whatever the platform (an arm64 fleet is dated
from arm64, decision 3). A sibling popping an already released seq is suppressed too (`skopeo copy --all` is one pull). An index still parked at `child_ttl` is released by the tick with `served = None`: a
row up to 60 s late, `published_at NULL`. The deque length is the count, so N index GETs buy exactly N child GETs, one per index, sequential or interleaved (a fleet pulling `nginx:latest`: index₁..indexₙ
then child₁..childₙ is N rows, not 2N−1 as a single consumed `Instant` would give), and a CI job's pinned `name@sha256:<child>` 1 s after another client's index pull records once that pull's own child
spent the count; only the first child GET after an index is indistinguishable from the pull that fetched it. A `docker pull` emits the index GET before the child GET and the channel is FIFO, so the index
is parked before its child is examined whatever the load. A bare digest pull with no recent index records on its own.

The cached npm body is the full packument (`NpmUpstream` sets no `Accept`, `npm/upstream.rs:13-52`): `time`, `versions[v].scripts` and upstream `dist.tarball` URLs are all
present (`packument.rs:10` and `strip_versions_to_abbreviated`, `:70-76`, touch only the served copy). `time[version]` from a hosted opencargo is `versions.published_at`
(`packument.rs:39`), a `datetime('now')` column (`001_initial.sql:35`): no `T`, no offset, hence `parse_time`'s fallback.

What a packument costs to read. Tens of MB for `@types/node` or `typescript`: `engine.bytes` reads the whole file and `serde_json` parses it, 10²–10³ ms for the largest, so once per tarball at `INFLIGHT =
64` would make one such package the writer's hot spot. `package_facts` parses **once per packument row, concurrent misses included**: `npm_facts` is a 1 024-entry LRU keyed `(member.id, name)` holding an `NpmSlot`. A call `peek`s the row
(one indexed read), takes the lock, keeps the slot whose `stamp == (row.id, row.fetched_at)` or replaces it with a fresh one (a refresh or re-fetch changes `fetched_at`), drops the lock, then
`cell.get_or_init(|| spawn_blocking(read + parse))`: the first caller parses, every concurrent caller awaits the same cell, and the parse runs off the HTTP workers. A plain check-then-fill memo
would not do: two hundred gather tasks for tarballs of one package all miss before any inserts, the engine's singleflight serialises only the fetch, and up to `INFLIGHT` tasks would then parse the
same tens of MB at once, 64 `Value` trees on the runtime that serves HTTP. The refresh is coalesced the same way: a caller whose `want` is absent finds `slot.refresh` `None` or older than `refresh_floor` and installs
`(now, cell)`, else awaits the existing cell (set when the refresh is done); the installer runs `refresh` and re-`peek`s, every waiter re-enters the memo under the new stamp, so a fleet pulling a version published
minutes ago is dated after one conditional request and one parse. Two hundred pulls of `lodash@4.17.21` read and parse it once, as the OSV memo answers them with one query (section 5.2). Why the row alone is not the fact: the packument is one row **per package** (`NpmArtifact::Metadata{name}`, `npm/upstream.rs:27-31`), so a copy cached at T knows nothing of a version published after
T, and a row-driven `peek` would answer `unknown` for exactly the young versions `min_release_age` exists to flag, for the whole TTL, since the row exists and a plain `fetch` on a fresh row is a cache hit.
Hence the miss rule in the table: `want` absent from `versions` -> `refresh` (`If-None-Match`; a 304 is headers only) at most once per package per `refresh_floor`, noted in the memo, so a tarball genuinely
absent from the packument (a mirror stem) costs one conditional request per 5 min, not one per pull. What it costs to fetch. `Metadata` is `CachePolicy::Ttl(Ttl::Default)` (`upstream.rs:38-44`) =
`proxy.default_ttl`, 24 h by default (`config.rs:172`), capped by the inherited `DEFAULT_MAX_UPSTREAM_BYTES` = 100 MB (`strategy.rs:6, 87-89`). On an `npm ci` (tarballs only) the recorder fetches one full
packument **per package per TTL window** plus one refresh per package whose version is newer than the copy: a 1 500-package lockfile on a cold packument cache is 1 500 upstream requests, multi-MB for the
largest, once a day; a cold `npm install` costs the proxy that today, `npm ci` did not, hence decision 3: fetch only when `min_release_age` or `install_scripts` is on (the other two need name and version
only); `fetch_missing_facts = false` keeps it at `peek`.

What a crates.io lookup costs. `CargoArtifact::VersionMeta { api: Url, name, version }` (`cargo/upstream.rs:20`): `upstream_url` = `{api}/api/v1/crates/{name}/{version}`, `url_source` = `UrlSource::Content
{ allow_private: up.dl_allow_private }`, `cache_key` kind `meta`, `CachePolicy::Immutable`, `MAX_METADATA_BYTES` (`strategy.rs:8`), `request_headers` = `User-Agent: opencargo/{CARGO_PKG_VERSION}`
(crates.io refuses requests without one; the proxy client sets none today). The sparse index carries no date, so this is the only source, and crates.io's crawler policy is one request per second with an
identifying agent: a first `cargo build` through a fresh proxy serves a few hundred `.crate` files, which at `INFLIGHT = 64` would be a few hundred API calls at 64 in flight, 429s and a throttled IP for
the member's *other* crates.io traffic. Hence `src/policy/pacer.rs`: `Pacer { lanes: Mutex<HashMap<host, Arc<Lane>>> }`, `Lane { gate: tokio::Mutex<Interval>, cooldown_until: Mutex<Option<Instant>>,
waiting: AtomicUsize }`, one lane per `api` host. `pacer.fetch(shared, cfg, member, up, a)`: `"rate-limited"` at once while `cooldown_until` is in the future or `waiting >= tuning.pacer_waiters()`
(`gather_timeout / pacer_period` = 15 at defaults); else it holds `gate` across `tick().await` (`tuning.pacer_period`, 1 s, `MissedTickBehavior::Delay`) **and** the request: one in flight, one second
apart, independent of `INFLIGHT`. A 429 (`transfer.rs:94` formats `"upstream answered 429 ..."`, `engine/mod.rs:159` wraps it in `BadGateway`; `pacer::is_throttled(&AppError)` matches that prefix) sets
`cooldown_until = now + tuning.pacer_cooldown` (60 s); the row is then `published_at NULL`, `date_source = "rate-limited"`, never `"fetch"`. Two deadlines, both `facts`'s, never the task's:
`timeout(gather_timeout, lane.wait())` on the line, `Elapsed` -> `"rate-limited"` without a request, then `timeout(gather_timeout, fetch)` on the request alone; so a slot is held at most 2 x
`gather_timeout` = 30 s whatever the line does. The bound is not the cap's: a holder keeps `gate` for `pacer_period` **plus** its request latency, so 15 waiters on a slow upstream would wait
`15 x (1 s + latency)`, minutes when crates.io drags; the wait deadline is what turns a waiter parked past 15 s into `rate-limited` and frees its permit, and the cap is only the point past which
joining the line is pointless (15 waiters at 1 s each fill the deadline on a fast lane; at 500 ms latency about ten get through, the rest time out). A cold `cargo build` therefore parks at most 16
slots (one holder, 15 waiters): its first crates are dated within ~15 s, the rest are `unknown` until a later pull of the same version tries again (a fetched date is cached `Immutable`), and 48
slots keep serving npm, go and OCI. A warm cache (`peek`) never touches the lane.

## 5. Rules

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)] #[serde(try_from = "String", into = "String")]   // src/policy/age.rs
pub struct Age(pub Duration);   // FromStr: `^\d+[smhd]$` after trim, "7d" = 604 800 s, else Err(String); Display echoes the source text ("48h")
#[derive(Debug, Clone, Deserialize)] #[serde(default, deny_unknown_fields)]   // src/policy/rules/mod.rs; no derived Default: a derived bool is false and `#[serde(default)]` would copy it
pub struct PolicyConfig { pub min_release_age: Option<Age>, pub osv_severity: Option<Severity>, /* low|medium|high|critical */ pub install_scripts: bool, pub typosquat: bool, pub fetch_missing_facts: bool }
impl Default for PolicyConfig { .. }   // hand-written like CleanupConfig (config.rs:192-200): rules off, `fetch_missing_facts: true`
impl PolicyConfig { pub fn is_empty(&self) -> bool; /* no rule on: nothing recorded */ pub fn needs_packument(&self) -> bool; /* age || scripts */ }
pub trait Rule: Send + Sync { fn name(&self) -> &'static str;  fn enabled(&self, cfg: &PolicyConfig) -> bool;
    fn evaluate(&self, cfg: &PolicyConfig, r: &Resolution, now: DateTime<Utc>) -> Option<RuleVerdict>; }  // pure, sync; None = deferred to flush (osv_severity only)
pub fn all_rules() -> Vec<Box<dyn Rule>>;   pub fn evaluate_all(rules, cfg, r, now) -> Vec<Option<RuleVerdict>>;   // sync: no network in the gather task
```
`Age` fails at config load (`#[serde(try_from)]`), so `min_release_age = "7 days"` or `"1w"` is a startup error naming the key and the accepted forms, where a bad `Severity`
already fails; nothing defaults silently. A `[policy.npm-proxy]` holding only `min_release_age = "48h"` deserialises with `fetch_missing_facts = true` (unit-tested through
`toml::from_str`): with a derived `Default` it would be `false` and `min_release_age` would answer `unknown` on every `npm ci`. `parse_since` (section 7) is `Age::from_str`
or RFC 3339. Only enabled rules run and only they get a verdict row; a rule off in config is absent from the report.

```toml
[policy.npm-proxy]            # key = the proxy member's repository name; a group name is refused at startup
min_release_age = "48h"       # Ns | Nm | Nh | Nd
osv_severity = "high"
install_scripts = true
typosquat = true
fetch_missing_facts = true    # false: cache-only facts, `unknown` otherwise; no recorder-initiated upstream request
```
`Config.policy: HashMap<String, PolicyConfig>` (`src/config.rs:14-26`, `#[serde(default)]`). `policy::startup::startup_notes(&Config) -> Result<StartupNotes, String>` is pure: `Err` names a hosted
or group key (`build_state` bails with it); `Ok(StartupNotes { recording, unknown: Vec<String>, inapplicable: Vec<(String, &'static str)> })` lists the members that record, keys naming no configured
repository (API-created ones may appear later) and `(oci member, rule)` pairs where `typosquat` or `osv_severity` is on (only `not_applicable` there); `build_state` logs one `warn` per non-empty list. Tests assert on the struct, never on captured logs (no test installs a subscriber; `main.rs:47` does).

### 5.1 `min_release_age` (`rules/min_release_age.rs`)
`published_at: None` -> `Unknown("no publish date ({date_source})")`. Else `now - published_at < min.0` -> `WouldBlock("published {age} ago, threshold {min}")`, else `Pass`. A future date (clock skew) is age 0, hence `WouldBlock`; the reason says so.
OCI dates are self-declared: `org.opencontainers.image.created` is the **build** time the publisher writes, and reproducible toolchains zero it (distroless and `ko` emit `1970-01-01T00:00:00Z`,
buildpacks `1980-01-01T00:00:01Z`, any `SOURCE_DATE_EPOCH` build a constant). Those parse, so `facts::oci_created` returns `None` with `date_source = unset-created` for anything before
2000-01-01 and the rule answers `Unknown`, never a `Pass` on a zeroed or forged stamp; a push time would need each registry's own API, out of scope (section 12).

### 5.2 `osv_severity` (`rules/osv_severity.rs`)
Per event (`Rule::evaluate`, pure): `format.osv_ecosystem()` (`src/db/kinds.rs:75-82`) `None` (oci) -> `NotApplicable`; `version: None` -> `Unknown("version unresolved")`; scanner disabled
(`vuln_scan.enabled = false`, `VulnScanner.osv == None`, `vulns/mod.rs:27-29`) -> `Unknown("osv scanning disabled")`; a memo hit -> `verdict(cfg, &finding)` (below); otherwise `None`, deferred.
Per flush (`evaluate_batch(scanner, memo, rows)` in the writer's spawned `flush`, section 3): the deferred `(ecosystem, name, version)` triples are deduplicated, grouped by ecosystem, each group one
call to a new `VulnScanner::assess_batch(&self, ecosystem, deps: &[(String, String)]) -> Result<Vec<Vec<VulnDetail>>, ScanError>` (the artifact's own advisories, through `query_batch` then
`advisories` with the id-level `AdvisoryCache`, `osv.rs:120-129`). `query_batch` takes no permit today (`osv.rs:141-179`; only `advisory` does, `:212`), so it gains `self.sem.acquire()`
around the POST: policy batches and publish-time scans share `vuln_scan.max_concurrency` (default 8, `config.rs:41,51`); a cold `npm ci` of 1 500 packages is ~24 POSTs of up to 64 queries,
at most 8 in flight. `Err(ScanError)` -> `Unknown("osv unreachable: {e}")` for the group.

The memo holds the **finding**, never advisory ids: `OsvFinding { top: Option<(String /* vuln_id */, Severity)>, at: Instant }` = the highest-severity `VulnDetail` of the artifact (`vuln_id`,
`severity`, `vulns/mod.rs:51-58`), `None` when clean, written by `evaluate_batch` from `assess_batch`'s own result; `Shared.osv_memo` is a 4096-entry LRU keyed `(ecosystem, name, version)` with a
1 h TTL, so a fleet pulling `lodash@4.17.21` two hundred times is one query. Ids would not do: severities live in `Advisory` behind `AdvisoryCache` (`osv.rs:83-97`), a private struct reached only
through the private `OsvClient.cache` (`:117-122`), FIFO-bounded at `CACHE_CAPACITY = 10 000` (`:14,108`) and so evictable, and `Rule::evaluate` is sync (section 5): a memo hit must carry
everything the verdict needs. Both stages call the same pure `verdict(cfg, finding) -> RuleVerdict`: `Some((id, sev))` with `sev >= cfg.osv_severity` (`Severity: Ord`, `severity.rs:13-21`) ->
`WouldBlock("{id} {sev} >= {level}")`, else `Pass` naming the top finding if any; `Severity::Unknown` sorts lowest and never reaches the threshold, stated in the reason. The threshold stays out of
the memo so one entry serves members with different `osv_severity` levels.

### 5.3 `install_scripts` (`rules/install_scripts.rs`)
`format != Npm` -> `NotApplicable`. `facts.install_scripts`: `None` -> `Unknown` naming the cause from `date_source` (`"packument not read ({date_source})"` or `"version
unresolved (filename-unparsed)"`), `Some(true)` -> `WouldBlock("declares preinstall/install/postinstall")`, `Some(false)` -> `Pass`. Pure: the fact was gathered by the recorder.
Loud by construction: `hasInstallScript` is true for esbuild, sharp, bcrypt, cypress, playwright, husky, so an ordinary lockfile yields dozens of `would_block` rows, the inventory an admin turns it on to see; the `rule=install_scripts` filter is the instrument, an allowlist a follow-up (section 12).

### 5.4 `typosquat` (`rules/typosquat.rs`, `distance.rs`, `lists/`)
Lists shipped via `include_str!`, one name per line, sorted, generated by `scripts/toplists.sh` (fetch date in the header):

| list | source | size | licence |
|---|---|---|---|
| `npm.txt` | `npm-high-impact` (wooorm, npm high-impact package list derived from download counts) | 5 000 names, ~65 KB | MIT |
| `crates.txt` | crates.io database dump, `crates` ordered by `downloads` | 5 000 names, ~50 KB | CC0-1.0 (dump data; confirm at import, see NOTICE) |
| `go.txt` | deps.dev module dependents ranking (BigQuery public dataset) | 2 000 module paths, ~70 KB, major suffixes stripped and deduplicated by `toplists.sh` | CC-BY-4.0 (attribution in NOTICE) |
| `known/{npm,crates,go}.txt` | the same deps.dev dependents ranking, top 20 000 per ecosystem (`toplists.sh --known`), exact-match `HashSet` only, never compared | ~300 KB each | CC-BY-4.0 |

The known lists exist because ecosystems name families one character apart: `mysql`/`mssql`, `es5-shim`/`es6-shim` (npm), `base64`/`base32` (crates), any Go path one substitution from a
popular one; every such name below the top 5 k would otherwise read as `Substitution of 'X'`, an accusation against a package the admin uses. The residual is stated, not hidden: a package
below the top 20 k that is one edit from a top-5 k name is still flagged, and the reason says `not in the top 20 k`. `format == Oci` -> `NotApplicable("oci: a misspelt official image is
never served")`: Docker Hub serves single-segment names only from `library/` (`upstream_name`, `oci/upstream.rs:66-73`), so a misspelling 404s, nothing is served, no event exists (decision 1).

`normalize(format, name) -> Normalized { scope: Option<String>, word: String }`: lowercase; `_` -> `-`; an npm `@scope/name` keeps both parts; go compares the module path **without its
major version**: a trailing `/vN` (N >= 2) and the `gopkg.in/...vN` form are stripped, so `github.com/go-redis/redis/v9` and `gopkg.in/yaml.v3` exactly match the list entry, never "1 edit
from `/v8`" (both lists are normalised the same way at generation and at load). Lists are parsed once into `Lists { unscoped, scoped: Vec<&str>, scopes: HashSet<&str>, known: HashSet<&str> }`
(`OnceLock`). In order: an exact match of a top name is `Pass("exact match of a top-N name")`; a name in `known` is `Pass("known package, not a squat")`; a scoped candidate whose scope is
in `scopes` is `Pass("scope @{scope} is itself top-N")` (`@babel/core` is not a squat of `cors`); a word under 5 characters is `Pass("too short for a 1-edit comparison")` (`ws`/`vs` are
ordinary names); otherwise a scoped candidate is compared as the full `@scope/name` against `scoped` only, an unscoped one against `unscoped` only, with `one_edit(candidate, top) ->
Option<Edit>` over top names of 5+ characters whose length differs by at most 1 (prefilter; 5 000 names is well under a millisecond). `Edit` is `Substitution` (`lodask`), `Transposition`
(`lodahs`) or `Separator`: a `-`/`.` of the top name **missing** from the candidate (`crossenv` vs `cross-env`, `lodashmerge` vs `lodash.merge`), never one added: `is-array`, `is-number`,
`object-assign` are real packages next to `isarray`, `isnumber`, `objectassign`, so `npm install is-array` must `Pass`; `one_edit` yields `Separator` only when `candidate.len() + 1 ==
top.len()`. A letter inserted or dropped is **not** an edit: `requests`/`request`, `colours`/`colors`, `asserts`/`assert` are distance-1 pairs and ordinary names, and with `request` in
every top-N list a plain OSA distance of 1 would flag every plural. A hit -> `WouldBlock("{edit} of '{top}', not in the top 20 k")`, none -> `Pass`. The FP measurement ships with the lists
and measures the residual, not the `known` short-circuit (every known name passes at check 2 by construction): `toplists.sh --known` also writes `lists/holdout/{npm,crates,go}.txt` (ranks
20 001–40 000, ordinary packages by definition, `#[cfg(test)] include_str!`, never in the binary); `typosquat_holdout_false_positive_rate` classifies each holdout, prints the `WouldBlock`
count per ecosystem and asserts it under a committed ceiling (`HOLDOUT_FP_MAX`, set from the first run); `known_list_suppresses_siblings` counts the known names one edit from a top name
(the rows the list saves), asserts it is above zero and prints it. Letter insertion/deletion (`lodas`, `loddash`), separator insertion and distance 2 wait for a real week of `rule=typosquat` traffic (section 12).

## 6. Storage: migration 014 and retention

```sql
-- src/db/migrations/014_policy.sql   (appended in migrate() after 013, src/db/mod.rs:133-134, with `?`)
CREATE TABLE IF NOT EXISTS policy_resolutions ( id INTEGER PRIMARY KEY AUTOINCREMENT, created_at TEXT NOT NULL DEFAULT (datetime('now')),
    requested_repo TEXT NOT NULL, member_repo TEXT NOT NULL, format TEXT NOT NULL, name TEXT NOT NULL, version TEXT, digest TEXT, published_at TEXT,
    actor TEXT NOT NULL, actor_kind TEXT NOT NULL CHECK (actor_kind IN ('token','user','static','anonymous')), user_id INTEGER,  -- users.id, the token's owner for a token; NULL for static/anonymous; no FK: history outlives the user, erasable by id
    would_block INTEGER NOT NULL DEFAULT 0, unknown INTEGER NOT NULL DEFAULT 0 );  -- any verdict would_block; else any verdict unknown
CREATE TABLE IF NOT EXISTS policy_verdicts ( resolution_id INTEGER NOT NULL REFERENCES policy_resolutions(id) ON DELETE CASCADE, rule TEXT NOT NULL,
    verdict TEXT NOT NULL CHECK (verdict IN ('pass','would_block','unknown','not_applicable')), reason TEXT NOT NULL DEFAULT '', PRIMARY KEY (resolution_id, rule) );
CREATE INDEX .. idx_policy_res_created (created_at);  idx_policy_res_user (user_id, created_at);  idx_policy_res_repo_created (requested_repo, created_at);  idx_policy_res_member_name (member_repo, name);  idx_policy_verdicts_rule ON policy_verdicts (rule, verdict);
```
Repository names are stored, not ids: a deleted-and-recreated proxy keeps its history readable, and the report filters by the name the admin knows. Actors are the opposite:
`actor` is a label for the page and `user_id` the key, because two users can each own a token named `ci-runner`, or one named after the other. `would_block`/`unknown` are
denormalised at insert so totals and pagination are one indexed scan; verdicts are fetched for the page's ids in a second query. Dates: `published_at` written as RFC 3339, `created_at` read back as `strftime('%Y-%m-%dT%H:%M:%SZ', created_at)`.

```rust
// src/policy/store.rs — all `pub async fn (pool: &SqlitePool, ..) -> Result<_, sqlx::Error>`
pub enum Subject { User(i64), Static }   // /me only: `user_id = ?` or `actor_kind = 'static'`
pub struct ReportFilter<'a> { pub since: DateTime<Utc>, pub repo: Option<&'a str>, pub rule: Option<&'a str>, pub subject: Option<Subject> }
pub async fn insert_batch(pool, rows: &[(Resolution, Vec<RuleVerdict>)]) -> Result<Vec<i64>, _>;   // one transaction
pub async fn report_totals(pool, f: &ReportFilter<'_>) -> Result<Totals, _>;   pub async fn list_resolutions(pool, f, page: i64, size: i64) -> Result<Vec<ResolutionRow>, _>;
pub async fn delete_older_than(pool, days: u64) -> Result<u64, _>;   pub async fn delete_by_user(pool, user_id: i64) -> Result<u64, _>;   // verdicts cascade// `repo` matches `requested_repo = ?1 OR member_repo = ?1`; `rule` joins `policy_verdicts` on that rule (rows where it was enabled); with it set, `totals.would_block`/`unknown`, each entry's `would_block`/`unknown` and its `verdicts` come from that rule's joined verdict alone (same index, same scan), never the denormalised columns, so tile, chip and `explain` agree with the filter; `by_rule` holds that rule only
```

Retention: `CleanupConfig.policy_report_older_than_days: Option<u64>` (`src/config.rs:183-200`, default `Some(90)`), applied in `run_cleanup` (`src/telemetry/cleanup.rs:52-74`) beside the proxy
sweep, like it regardless of `cleanup.enabled`; `CleanupStats` gains `policy: Option<u64>`; `0` or absent disables it. The start guard at `cleanup.rs:38-41` becomes a pure `fn sweeps_configured(config)
-> bool` = `enabled || proxy_idle_days().is_some() || policy_days().is_some()`, so `enabled = false, proxy_cache_older_than_days = 0, policy_report_older_than_days = 30` still starts the task. Both are proven in-crate (`cleanup::tests`; `fixture()` at `cleanup.rs:208` runs `migrate`): `start_cleanup_task` is spawned only by `main.rs:83`, never by `spawn_in` (`tests/common/mod.rs:98-120`), and `run_cleanup` is `pub(crate)`, so no `tests/` case can drive a sweep.

## 7. API (`src/api/policy.rs`, routes in `src/server.rs:217-337` next to `/api/v1/system/audit` `:277`)

```
GET    /api/v1/policy/report?since=24h|2026-09-16T00:00:00Z&repo=npm-all&rule=min_release_age&page=1&size=50   (admin)
DELETE /api/v1/policy/report?user=alice | ?user_id=17   -> { "deleted": 41 }; audited as `policy.erase`, target `deleted=41`, never the name   (admin)
GET    /api/v1/policy/rules                                                                                     (admin)
GET    /api/v1/me/policy?since=&repo=&rule=&page=&size=   the caller's own rows, same shape as /report       (any authenticated user)
```
`audit_log` has no sweep (`run_cleanup`, `cleanup.rs:52-74`, handles pre-releases and the proxy cache only), so an erasure audited with the actor's name would keep it past the 90-day bound in a second
table; the entry records who erased, when and how many rows, nothing about whom (a hash of a username from a small known set is reversible). `DELETE ?user=` resolves the name through `get_user_by_username`
(`db/mod.rs:484`, 404 when unknown), `?user_id=` erases a user already deleted (the id survives in `audit_log.user_id`); both call `delete_by_user`, so bob's token named `ci-runner` never goes with ci's
rows. `/me/policy` is the data subject's view: `ReportFilter.subject` is forced to `User(caller.user_id)` for a DB user (Basic auth and every token they own, revoked ones included) or `Static` for a config
token; no query key widens it, no admin needed, and `actor` is never a filter: bob naming a token `alice` gets his own rows only.
```rust
#[derive(Deserialize)] pub struct ReportQuery { since: Option<String>, repo: Option<String>, rule: Option<String>, page: Option<i64>, size: Option<i64> }
fn parse_since(s: Option<&str>, now: DateTime<Utc>) -> AppResult<DateTime<Utc>>; // Age ("24h", "7d") or RFC 3339; default 24h; 400 otherwise
```
Caller via `require_auth` + `require_admin` (`src/api/mod.rs:19-35`); `page.max(1)`, `size.unwrap_or(50).clamp(1, 200)` as `audit.rs:43-44`; an unknown `rule` is 400 listing
the four names; `DELETE` with neither `user` nor `user_id`, or both, is 400. Response:
```json
{ "since": "2026-09-16T10:00:00Z", "page": 1, "size": 50, "process": { "dropped_since_start": 0 },
  "totals": { "resolutions": 212, "would_block": 9, "unknown": 40, "by_rule": { "min_release_age": { "would_block": 7, "unknown": 40, "pass": 165 }, "osv_severity": { .. } } },
  "entries": [ { "id": 981, "created_at": "2026-09-17T09:12:03Z", "requested_repo": "npm-all", "member_repo": "npm-proxy", "format": "npm", "name": "left-pad", "version": "1.3.1",
                 "digest": "sha256:..", "actor": "ci-runner", "actor_kind": "token", "user_id": 3, "published_at": "2026-09-17T07:10:00Z", "would_block": true, "unknown": false,
                 "verdicts": [ { "rule": "min_release_age", "verdict": "would_block", "reason": "published 2h ago, threshold 24h" } ] } ] }
```
`process.dropped_since_start` is the process-lifetime `PolicyEngine.dropped` counter, deliberately outside `totals`, whose every number is scoped by `since`/`repo`/`rule`: the
shape says it is not. `/rules` answers `{ "osv_enabled": bool, "recording": ["npm-proxy"], "repositories": { "npm-proxy": { "min_release_age": "48h", "osv_severity": "high",
"install_scripts": true, "typosquat": true, "fetch_missing_facts": true } } }` for every proxy repository, defaults included, so the page can label "off" rules and say which
members record. `docs/api.md` gains the four routes after the audit entry (`:174`), the per-member gating note, and the `policy.resolution` WS event (`{repo, member, count,
would_block, unknown}`, one per flush per repo pair, at most two a second per pair, admin) in the event table (`:273`). `README.md` `[cleanup]` (`:340-343`) gains `policy_report_older_than_days = 90` and `[policy.<repo>]` with
three sentences: a rule on records actor name, artifact and time of every download through that member for that many days; `/me/policy` shows one's own rows; `DELETE` erases one user's rows, audited without the name.

Where `token_name` comes from, at no added query. The DB-token path already holds the row (`db_token.name`, `try_db_token_auth`, `middleware.rs:407-420`). The registry-token path (every OCI
manifest and blob GET) runs `api_token_is_live(&state.db, id)` today (`:217`; `SELECT expires_at FROM api_tokens WHERE id = ?1`, `:440-446`); it becomes `live_api_token_by_id(db, id) -> Result<Option<ApiToken>, _>`
(`None` when missing or expired) and `token_name` is `row.name`: the same single statement, no `get_token_by_id` after it. `from_user(user, token)` gains a `token_name: Option<String>` argument at its three callers (`:196` Basic, `:236` registry, `:419` DB token).

## 8. UI: "Policy report"

- `frontend/src/core/types.ts`: `PolicyVerdict`, `PolicyEntry`, `PolicyTotals`, `PolicyReport`, `PolicyRules` (after `AuditResponse`, `:175-192`); `core/api.ts`: `fetchPolicyReport(q: PolicyQuery)` (`URLSearchParams`), `fetchPolicyRules()` (next to `fetchAudit`, `:201-203`).
- `frontend/src/core/stores/policy.ts`: `createPolicyStore()` holding `since` (`'24h'` default), `repo`, `rule`, `page` signals; `query()` memo -> `PolicyQuery`; `createResource(query, fetchPolicyReport)`;
  `useLive(refetch, ['policy.resolution'], { debounce: 300, maxWait: 2000 })`: `useLive` re-arms its timer on every event (`stores/live.ts:29-36`), so a debounce above `notify_period` (500 ms) would never fire while frames keep coming; `LiveOpts` gains `maxWait?: number`, a deadline set at the first event of a burst that refetches whatever the re-arming, then clears; a filter change resets `page` to 1. Pure exports `toQuery(filters)` and `explain(entry)` (would_block reasons joined by `;`, else `unknown:`, else `passed`).
- `frontend/src/pages/admin/PolicyReport.tsx`: `RequireAdmin` guard (`AuditLog.tsx:20-26`), page head with the live dot (`:74-77`), a filter bar (since 1h/24h/7d/30d, repo, rule), three stat tiles
  (resolutions, would block, unknown), a banner "N events dropped since start" when `process.dropped_since_start > 0`, an empty state naming the recording members, then `table-card/table-scroll/table`
  (`:123-163`): When, Actor, Artifact (`name@version`, `format` chip), Repo (`requested -> member`), Verdict chip (`chip-danger`/`chip-warn`/`chip-ok`), Why (`explain(entry)`); Newer/Older pagination
  (`:165-181`). Erasure and `/me/policy` have no UI at launch: one `curl` each. Route `/admin/policy` in `frontend/src/index.tsx:79` and `src/web/mod.rs:107` (mirrored lists); sidebar link "Policy report", icon `shield` (`components/Sidebar.tsx:111-118`).

## 9. Tests as proof

Harness additions (`tests/common/mod.rs`): `SpawnOpts.policy: HashMap<String, PolicyConfig>` (into `Config.policy` in `test_config`, `:53-82`) and `policy_tuning: Option<Tuning>` (applied by
`spawn_in` replacing `state.policy` with `PolicyEngine::new_tuned` before `build_router`, `:106-109`, section 3); `named_token(client, base_url, username, name) -> String` = `create_user` (`:407-427`)
then `POST /api/v1/users/{u}/tokens {"name"}` (`src/api/tokens.rs:62-122`); `backdate_version(server, package, version, hours)` = `UPDATE versions SET published_at = datetime('now', '-N hours')`
on `tmp/opencargo.db` like `expire_entries` (`:200-214`); `policy_rows(server) -> Vec<(actor, name, version, would_block)>` by SQL like `cache_rows` (`npm_proxy_test.rs:110-122`);
`wait_for_policy_rows(server, n)` polls `policy_rows` every 50 ms until `len() >= n` or 5 s, then asserts `== n` (rows are written after the response; every HTTP-driven case reads them only
through it); `report(server, query) -> Value` with `STATIC_TOKEN`. Absence is never proven with a sleep: after the pulls that must not record, the case pulls one known-recording artifact
(the **sentinel**) and waits for its row; the channel is FIFO and the writer receives in order, so the sentinel's row proves every earlier event was examined. `fake_upstream/npm.rs` (`:37`) gains
`FakeNpm::add_tarball(filename, bytes)` serving `/{name}/-/{filename}` (today `serve` 404s every path but `/{name}`, `:71`), `add_version`, `ETag`s and `set_latency(Duration)`; `fake_upstream/oci.rs` gains `add_index(name, tag, children)`; `fake_upstream/cargo.rs` the version API with `set_api_status` and timed `hits`.

`tests/policy_test.rs` (offline; each case spawns its own upstream as the `*_proxy_test.rs` files do and enables at least one rule, otherwise nothing records):

| case | setup | asserts |
|---|---|---|
| `npm_two_tokens_two_rows_with_dates` | second instance + tap (`npm_proxy_test.rs:57-69`), `backdate_version(2h)`, policy `min_release_age = "48h"` and `install_scripts = true` (the version declares no script), tokens `ci-runner` and `dev-laptop` pull `@acme/widget-1.0.0.tgz` via `npm-proxy` | `wait_for_policy_rows(2)`, actors by token name, `version = 1.0.0`, `digest` set, `published_at` 2 h old (parsed from the SQLite shape), both `would_block`; report `totals.would_block == 2`, `by_rule.min_release_age.would_block == 2`; `repo=npm-proxy` and `rule=min_release_age` keep both, `repo=nope` none; `rule=install_scripts` keeps both rows with `totals.would_block == 0`, every entry `would_block == false` and `verdicts == [install_scripts: pass]`: the tile and chips follow the filtered rule, not the denormalised columns |
| `npm_ci_without_packument_still_dated` | fake npm with `add_tarball`, `min_release_age = "48h"`; pull the tarball URL directly, never the packument | 1 row with `published_at` from `time[version]`, `date_source = fetch`, fake `hits` show exactly one `/{name}` request from the recorder |
| `npm_version_newer_than_cached_packument_refreshes` | fake npm with `ETag`s; pull `1.0.0` (packument cached fresh); `add_version("1.1.0", time)` + `add_tarball` on the fake (new `ETag`), pull the `1.1.0` tarball directly; `add_tarball("1.2.0")` **without** touching the packument, pull it twice | `1.1.0` dated, `date_source = refresh`, the fake saw one more `/{name}` request carrying `If-None-Match`; `1.2.0`: one conditional request answered 304, `published_at NULL`, `not-in-packument`; the second pull, inside `refresh_floor`, made no request; the fake's `/{name}` hits total 3 for 4 pulls |
| `npm_ci_fetch_gated_on_rules` | same fake, three `respawn`s: `typosquat = true` only; `install_scripts = true` with `fetch_missing_facts = false`; `install_scripts = true` | zero `/{name}` requests and `date_source = none`; zero and `install_scripts = unknown` with reason `not-fetched`; one request and a verdict |
| `npm_tarball_stem_without_name_prefix` | fake packument whose `dist.tarball` is `/{name}/-/renamed-1.0.0.tgz`, `add_tarball("renamed-1.0.0.tgz")`, `install_scripts = true` | `version = 1.0.0` resolved through `dist.tarball`; a second tarball absent from the packument gives `version NULL`, `date_source = filename-unparsed`, reason `version unresolved` |
| `npm_install_scripts_from_cached_packument` | fake npm with `scripts.postinstall` on one version and `add_tarball` for two versions, policy `install_scripts = true` | `wait_for_policy_rows(2)`; verdict `install_scripts = would_block` with reason naming `postinstall`; the version without scripts passes |
| `cargo_row_dates_from_api_or_unknown` | fake index (`fake_upstream/cargo.rs:69`, `add_crate`) whose `config.json` `api` points at the fake, which serves `/api/v1/crates/{n}/{v}` with `created_at`; policy `min_release_age = "1h"`, two tokens; a second crate the fake API 404s | 2 rows `would_block` with `published_at == created_at`, the API asked once for two pulls; the 404 crate is `published_at NULL`, `min_release_age = unknown`; the API request carried a `User-Agent` |
| `cargo_api_is_paced_and_429_is_unknown` | same fake, `policy_tuning.pacer_period = 500 ms`, `pacer_cooldown = 2 s` (cap 30 waiters, above the 6); 6 distinct crates pulled back to back, then `set_api_status(429)` and 2 more pulls, then `set_api_status(200)` and 1 pull 2 s later | the fake's 6 API hit timestamps are >= 500 ms apart and never overlap (`hits` carry start/end); the 429 row is `date_source = rate-limited`, the pull right after it made **no** API request (cooldown), the last one did and is dated; `date_source` is never `fetch` for an undated row |
| `go_row_reads_time_from_cached_info` | second instance + fake GOPROXY (`go_proxy_test.rs:47-70`), `.info` fetched then `.zip` | rows only for `.zip` (2, not 6), `published_at == Time` |
| `oci_manifest_get_records_head_does_not` | fake registry `Options` Hub-shaped (`fake_upstream/oci.rs:29`), manifest with `created` annotation, `docker`-like HEAD then GET, two tokens | `wait_for_policy_rows(2)`, `version = tag`, `digest = sha256:..`, `published_at` from the annotation, `osv_severity = not_applicable`, `typosquat = not_applicable` |
| `oci_index_pull_is_one_row_dated_from_served_child` | `add_index` over amd64 and arm64 manifests without annotations, config blobs with distinct `created`; 20 pulls back to back, each = GET index by tag then GET the amd64 manifest by digest, then one arm64 pull; then two pulls interleaved by two tokens (index, index, child, child); then, 1 s after one more index pull by token A, token B pulls the amd64 child by bare digest; then a bare pull of the other child later than `policy_tuning.child_ttl` | after the burst exactly 21 rows and after the interleaved pair exactly 23 (a sentinel npm pull proves the children were examined), `version = tag`, 20 rows dated from the amd64 config and one from the arm64 config, `date_source = config-blob`; the fake's `manifests/` hits are exactly the clients' (the recorder issued none); B's digest pull adds one row of its own (A's child spent the count), the late pull adds one keyed by digest |
| `oci_index_without_child_is_unknown_after_ttl` | `policy_tuning.child_ttl = 1 s`; one index GET by tag, nothing else, then a sentinel | one row after ~1 s, `published_at NULL`, `date_source = index-unpulled`, `min_release_age = unknown`; a config blob with `created = 1970-01-01T00:00:00Z` pulled as a plain manifest gives `unset-created` and `unknown` |
| `osv_rule_uses_fake_osv_and_id_cache` | `fake_osv::start` (`tests/common/fake_osv.rs:32`), `affect("npm", name, version, ["GHSA-x"])`, `record(cvss_record(.., V3 9.8))`, policy `osv_severity = "high"`, 3 pulls of one version then 70 pulls of distinct versions | `osv_severity = would_block` on the first three rows, `osv.hits("GHSA-x") == 1`, one `querybatch` carrying one query for the three pulls (dedupe + memo); the 70 distinct versions cost at most 2 `querybatch` POSTs (one per flush), none with more than 64 queries; `set_down(true)` then a pull -> `unknown` |
| `hosted_reads_record_nothing` | `npm-hosted` and group `npm-all = [npm-hosted, npm-proxy]`, `[policy.npm-proxy]` on; first pull an upstream-only package via the group, then publish `@acme/internal` to hosted and pull it via `npm-hosted` and via `npm-all`, then a sentinel proxy pull | `wait_for_policy_rows(1)` with `requested_repo = npm-all`, `member_repo = npm-proxy`; after the sentinel `wait_for_policy_rows(2)`: the hosted pulls left no row |
| `unconfigured_member_records_nothing` | `npm-proxy` with no `[policy.*]` at all, one pull; `respawn` with `install_scripts = true`, then a sentinel pull | `wait_for_policy_rows(1)` after the respawn and the row is the sentinel, not the first pull |
| `rules_endpoint_reports_effective_config` | one configured proxy, one API-created proxy | both listed, `recording` names only the first, defaults off for the second, `min_release_age` echoed as `"48h"`; non-admin token -> 403 |
| `policy_key_on_group_refused` | `seed_error_opts(SpawnOpts { policy: [("npm-all", ..)], .. })`: `seed_error` (`:134-147`) takes repositories only, so it gains an opts twin it delegates to | error names the repository |
| `erase_user_deletes_rows_and_audits` | user `ci` with token `ci-runner`, user `bob` with a token **also named** `ci-runner`, two pulls each; `DELETE /api/v1/policy/report?user=ci` | `deleted == 2`, bob's rows and verdicts intact although their `actor` is `ci-runner` too; one `policy.erase` audit entry whose `target` is `deleted=2` and whose row contains neither `ci` nor `ci-runner`; then delete user `ci`, `?user_id=<id>` -> `deleted == 0` and 200; `?user=nobody` -> 404; non-admin -> 403; no key -> 400 |
| `me_policy_shows_only_own_rows` | user `alice` with token `dev-laptop` and a Basic-auth pull; user `bob` with tokens `ci-runner` **and `alice`**; one pull per credential | `GET /api/v1/me/policy` as `alice` lists exactly her two rows (`dev-laptop`, `alice`); as `bob` exactly his two (`ci-runner`, `alice`): the label `alice` appears in both views, the rows never cross; `?actor=`/`?user_id=` keys are ignored; `STATIC_TOKEN` sees only `actor_kind = static` rows; anonymous -> 401 |
| `oci_registry_token_row_names_token_and_owner` | registry token issued to user `ci`'s token `ci-runner` (`/v2/token` flow of `oci_auth_test.rs`), one `docker`-like pull | the row has `actor = ci-runner`, `actor_kind = token`, `user_id = ci`; the zero-added-query claim is by construction (`run_registry_token` keeps its single `api_tokens` statement) and `grep get_token_by_id src/auth` stays empty |

Not in `tests/`: startup warnings are `policy::startup::tests::{notes_name_recording_and_inapplicable_members, group_key_is_err}` over `startup_notes(&Config)` from `toml::from_str`
(`[policy.npm-proxy]` on, `[policy.oci-proxy]` with `typosquat = true` -> `recording == ["npm-proxy", "oci-proxy"]`, `inapplicable == [("oci-proxy", "typosquat")]`); the bad age is
`age::config_rejects_bad_age` alone (`seed_error` builds a `Config` struct, `:138-141`; a `#[serde(try_from)]` error exists only when TOML is parsed); retention is
`cleanup::tests::policy_sweep_deletes_old_rows` (`fx.pool`, rows 100 days back by SQL, `run_cleanup` with `enabled: false, policy_report_older_than_days: Some(30)` -> old rows gone, recent kept, verdicts cascaded under `foreign_keys=ON` `db/mod.rs:81`, `stats.policy == Some(n)`; `Some(0)` -> `None`).

In-crate unit tests: `age::{parses_s_m_h_d (7d = 604800), rejects_words_weeks_and_spaces, display_roundtrips, config_rejects_bad_age (toml::from_str::<Config> error names the key and lists s/m/h/d)}`;
`rules::{config_default_fetches_missing_facts (toml::from_str::<PolicyConfig>(r#"min_release_age = "48h""#).fetch_missing_facts is true, and PolicyConfig::default().is_empty()), config_rejects_unknown_key}`;
`rules::min_release_age::{unknown_without_date, young_is_would_block, old_passes, future_date_is_would_block, seven_days_from_config_is_seven_days}` (the last through `PolicyConfig` deserialisation);
`rules::install_scripts::{non_npm_not_applicable, none_is_unknown_naming_cause, true_blocks}`; `rules::typosquat::{exact_top_name_never_flagged, substitution_flagged (lodask), transposition_flagged
(lodahs), separator_dropped_flagged (crossenv vs cross-env, lodashmerge vs lodash.merge), separator_added_passes (is-array vs isarray, is-number vs isnumber, object-assign vs objectassign, in a list
holding only the concatenated spelling), plural_suffix_passes (requests, colours, asserts, moments), letter_dropped_passes (lodas), scoped_top_scope_passes (@babel/core vs cors), scoped_only_against_scoped,
underscore_dash_normalised, go_major_version_bump_passes (redis/v9 vs list redis/v8, yaml.v3 vs yaml.v2), go_one_edit_in_base_path_flagged, short_names_not_compared (ws vs vs), known_sibling_passes
(mssql, es5-shim, base32 in `known`, not in top), typosquat_holdout_false_positive_rate, known_list_suppresses_siblings, oci_not_applicable}`; `rules::osv_severity::{oci_not_applicable, disabled_scanner_is_unknown,
unresolved_version_is_unknown, threshold_is_inclusive, memo_hit_is_not_deferred (a memo entry `Some(("GHSA-x", High))` answers `Some(would_block)` from `evaluate` alone under `osv_severity = "high"` and `Some(pass)` under `"critical"`; a `None` finding is `pass`; an expired entry is `None`, deferred), batch_groups_by_ecosystem_and_dedupes (64 npm rows of 3 versions + 3 cargo rows = 2 POSTs, 3 + 3 queries; the memo then holds the top `(vuln_id, severity)` per triple), batch_error_marks_every_row_unknown}`
(the last five against an in-crate `FakeOsv`); `distance::{one_edit_table (candidate/top: ab/ba = Transposition, abc/abd = Substitution, abc/ab-c = Separator, ab-c/abc = None, abc/abcd = None, abc/xyz = None), equal_strings_are_not_an_edit}`;
`facts::{parse_time_rfc3339_with_millis, parse_time_sqlite_shape_is_utc, parse_time_garbage_is_none, npm_version_scoped_and_unscoped, npm_version_from_dist_tarball_when_stem_differs, npm_version_none_without_packument_match,
package_facts_parses_once_per_row (two sequential calls share one `Arc`; 64 concurrent calls on a cold memo, the fixture route delayed 200 ms, parse once by a `#[cfg(test)]` parse counter and all get the same `Arc`; a bumped `fetched_at` re-parses; 64 concurrent calls with a missing `want` issue one `If-None-Match` request and re-parse once; a second miss inside `refresh_floor` makes no request), oci_created_from_annotation, oci_created_before_2000_is_none,
oci_blob_uses_upstream_name (`Upstream` on `https://registry-1.docker.io`: `nginx` -> `Blob { name: "library/nginx" }`, `acme/app` unchanged; on `http://127.0.0.1:1` the identity; the only proof, section 4),
oci_children_of_index_skips_attestations, digest_comes_from_source_for_every_format}`; `policy::{actor_of_token_user_static_anonymous (kind, label and `user_id` for each `AuthUser` shape, `token_name` set with `user_id` ->
Token carrying the owner's id), record_is_noop_without_rules (no clone: `records` false, the `source` closure never runs), full_queue_drops_and_counts}` (`unspawned`, the writer future never polled, `QUEUE + 1` sends: `dropped == 1`; the counter, not steady state);
`writer::{batch_inserts_in_one_transaction, flush_emits_one_coalesced_event (300 events in one burst, `notify_period = 200 ms`, a bus subscriber: exactly two `policy.resolution` frames, an immediate and a trailing one, whose `count`s sum to 300; a second burst 1 s later: one more), oci_child_after_index_in_same_burst_is_suppressed (50 index+child pairs sent back to back, 50 rows, each `served` the child), child_consumes_key_once (index, child, child again: 2 rows), stale_child_key_records_pull (`oci_classify` on a `Shared` whose `recent_children` was seeded with a seq stamped `2 x child_ttl` ago and no tick ever run: the child is `Some`, the key gone), interleaved_pulls_each_suppress_one_child (index, index, child, child: 2 rows; a third child: 3), child_before_index_records_both, parked_index_released_at_ttl (index alone, `child_ttl` 100 ms: one row, `index-unpulled`), idle_writer_is_not_repolled (the `unspawned` future in a poll
counter, `tokio::time::pause`, an open empty channel, 1 000 `yield_now`s: polled at most twice), gather_timeout_writes_unknown_row (packument route `delay = 2 x gather_timeout`: row written with `date_source = timeout`, the slot freed at ~`gather_timeout`),
flush_never_blocks_receive (a `FakeOsv` holding one batch for 2 s while 300 more events arrive: all 300 rows written before it answers), burst_over_cold_rate_drops_nothing}`. The last three drive `unspawned` against
`src/proxy/engine/fixture.rs` (in-crate; `tests/common` is unreachable from `src/`), whose `Fx`, `FakeState`, `Strat`, `timeouts` are `pub(super)` today (`fixture.rs:18,27,85,160,169`) and become `pub(crate)` in
commit 1, **and** the module itself, `#[cfg(test)] mod fixture;` at `engine/mod.rs:360-361`, becomes `#[cfg(test)] pub(crate) mod fixture;` (`pub mod engine` at `proxy/mod.rs:8` already allows it): a private module
leaves `crate::proxy::engine::fixture` unnameable from `policy` whatever its items say, so `policy::writer::tests` can build a `Shared` over `fx.engine(timeouts())` and `fx.pool` only with both changes; `FakeState.delay` (`:24`) exists and the fixture gains a packument-shaped
`/{name}` route, `gather_timeout` shrunk through `Tuning`. The burst case is `#[ignore]` (5 000 events at 500/s for 10 s over 5 000 **distinct** versions, `delay = 200 ms`, `fetch_missing_facts` on, so every gather is
cold: asserts `dropped == 0`, backlog peak `< QUEUE` from `tx.capacity()`, 5 000 rows eventually; ~16 s, `make test-load`, not `cargo test`);
`pacer::{one_in_flight_one_second_apart (period 50 ms, 10 fetches: timestamps monotone, >= 50 ms apart), throttled_sets_cooldown_and_rate_limited, waiters_beyond_cap_are_rate_limited_at_once (cap = `gather_timeout / pacer_period`: 15 at defaults, 4 at 2 s / 500 ms; the fifth waiter is `rate-limited` at once), wait_past_gather_timeout_is_rate_limited (a holder whose request sleeps 3 x `gather_timeout`: the waiter returns `rate-limited` at ~`gather_timeout` with no request of its own, its permit freed)}`;
`cleanup::{sweeps_configured_by_policy_bound_alone, policy_sweep_deletes_old_rows}`; `auth::middleware::tests::live_api_token_by_id_returns_name_and_rejects_expired`;
`engine::tests::{peek_never_hits_upstream, refresh_never_records_a_miss (a fresh `200` row, the fixture route switched to 404, then 502: both `refresh` calls answer `NotFound`, the row is still `status 200` with its file on disk and `fetch` still serves it; the same route under `fetch` writes the 404 row)}` (fixture from `engine/fixture.rs`). Frontend: `frontend/src/core/stores/policy.test.ts` (vitest, `vi.mock('../api.ts')` and the `ws.ts`
bus mock of `stores/live.test.ts:73-98`): `toQuery` serialises only set filters; changing `repo` resets `page` to 1; a `policy.resolution` event triggers one debounced refetch; `live.test.ts` gains `max_wait_fires_under_sustained_events` (an event every 100 ms for 3 s with `debounce: 300, maxWait: 2000`: a refetch by 2 s, another by 4 s);
`explain` prefers would_block, then `unknown:`, then `passed`; the banner renders only when `dropped_since_start > 0`. Makefile `test-quick` (`:52-56`) adds `--test policy_test`; `test-load` runs the `#[ignore]` burst case.

## 10. Launch demo scenario

`examples/policy-demo.toml`: `npm-hosted` (private, `@acme/*`), `npm-proxy` -> `https://registry.npmjs.org`, `npm-all = [npm-hosted, npm-proxy]`, `[policy.npm-proxy]` all four
rules on (`min_release_age = "24h"`, `osv_severity = "high"`), `vuln_scan.enabled = true`; tokens `ci-runner` (user `ci`), `dev-laptop` (`alice`).

1. Pick a package version published about two hours ago (`npm view <pkg> time --json`), or the deterministic variant: a second opencargo as upstream with `backdate_version(2h)`
   exactly as `npm_two_tokens_two_rows_with_dates` (SQLite-shaped `time[version]`, accepted by `parse_time`).
2. `npm install <pkg>@<version> --registry http://host/npm-all` once with each token (`.npmrc` `//host/:_authToken`). Both succeed: nothing is blocked. The Policy report page
   shows two rows within two seconds (one coalesced WS `policy.resolution`, then the 300 ms debounce), actors `ci-runner` and `dev-laptop`, verdict `would_block` with "published 2h ago, threshold 24h". Repeat with
   `npm ci` from a lockfile: same rows, the recorder fetches the packument the client never asked for.
3. `npm publish @acme/internal` to `npm-hosted`, then `npm install @acme/internal` via `npm-all` with either token: no row; the group answered from hosted (`walk`, `resolve.rs:173`).
4. Filter `rule = osv_severity` (`pass` or `would_block` per package); `rule = typosquat` on `npm install lodahs` (a real npm 404 records nothing), `npm install @babel/core` (`pass`, "scope
   @babel is itself top-N"), `npm install mssql` (`pass`, "known package") and `npm install requests` (`pass`: a letter appended is not an edit); `rule = install_scripts` on the lockfile shows esbuild-class packages as `would_block`, read as a count.
5. As `alice`, `curl -u alice /api/v1/me/policy`: her own rows, nobody else's. The demo is npm only; a non-npm member leaves `typosquat` and `osv_severity` off (startup warns).

## 11. Delivery plan (one implementer, sequential, suite green after each)

| commit | files | proof |
|---|---|---|
| 1 `feat(policy): record proxy resolutions` | `db/migrations/014_policy.sql`, `db/mod.rs` (migrate, `Repository: Clone`), `policy/{mod,age,writer,facts,store,startup}.rs` (`facts::NpmSlot` over `tokio::sync::OnceCell`, `Cargo.toml:15` `full`), `policy/rules/mod.rs` (trait + `PolicyConfig` with `Age` and `fetch_missing_facts`, no rules yet; recording gated on `is_empty`), `config.rs`, `auth/middleware.rs` (`token_name` at `:39-58`, `from_user` + argument at `:196`, `:236`, `:419`; `static_token_user` `:387-388` the only other literal; `api_token_is_live` `:440-446` becomes `live_api_token_by_id` returning the row, `:217` reads `name` from it; `api/ws.rs:214` is a `..` pattern, untouched), `lib.rs` (`pub mod policy`), `proxy/engine/mod.rs` (`peek`, `refresh`, `fetch` split into `lookup`/`exchange`/`settle` with a `Miss` mode, `#[cfg(test)] pub(crate) mod fixture;` at `:360-361`), `proxy/engine/fixture.rs` (`pub(super)` -> `pub(crate)`), `proxy/engine/payload.rs` (`Cached: Clone`, `:18`), `registry/cargo/upstream.rs` (`VersionMeta`), `policy/pacer.rs` (`Pacer`, `Tuning`), four `leaves.rs` (`records`-gated `source` closure over the `Found(cached)` before `into_payload`; npm and go copy `entry.digest`, oci gains `fetch_cached` for the `!head` GET and clones the `Cached`), `server.rs` (`AppState.policy`, `PolicyEngine::new` spawns the writer inside `build_state`; signature unchanged, `main.rs` and the fourteen callers untouched, `startup_notes` warns and bail), `telemetry/metrics.rs`, `tests/common/mod.rs` (`wait_for_policy_rows`, sentinel helper, `seed_error_opts`, `policy_tuning` through `new_tuned` in `spawn_in`), `tests/common/fake_upstream/{npm,oci,cargo}.rs` (`add_tarball`, `set_latency`, `add_index`, version API with `set_api_status`), `tests/policy_test.rs` (`npm_two_tokens_two_rows_with_dates` without verdict asserts, `npm_ci_without_packument_still_dated`, `npm_version_newer_than_cached_packument_refreshes`, `npm_ci_fetch_gated_on_rules` rows only, `npm_tarball_stem_without_name_prefix` rows only, `cargo_row_dates_from_api_or_unknown` rows only, `cargo_api_is_paced_and_429_is_unknown`, `go_row_reads_time_from_cached_info`, `oci_manifest_get_records_head_does_not` rows only, `oci_index_pull_is_one_row_dated_from_served_child`, `oci_index_without_child_is_unknown_after_ttl` rows only (`published_at NULL`, `date_source` in `index-unpulled` / `unset-created`), `oci_registry_token_row_names_token_and_owner`, `hosted_reads_record_nothing`, `unconfigured_member_records_nothing`, `policy_key_on_group_refused`), unit `age::*`, `facts::*`, `rules::config_*`, `policy::*`, `startup::*`, `writer::*` (incl. the ordering, idle and load cases), `pacer::*`, `peek_never_hits_upstream`, `refresh_never_records_a_miss`, `live_api_token_by_id_*` | `cargo test` green with `build_state` untouched for its callers; rows exist with actor label, `user_id`, date, digest for all four formats; hosted and unconfigured members record nothing (sentinel-proven); one row per multi-arch pull under a 20-pull burst and under interleaved pulls, dated from the served child with no recorder manifest request, a digest pull beside it records; a version newer than the cached packument is dated after one conditional refresh; one WS frame per flush; an idle writer is not re-polled; crates.io calls are one at a time, a second apart, 429 cools down; a bad age refuses config parsing; a rule-only config fetches facts; a 10 s burst at 500/s drops nothing (`make test-load`); no `/v2/` request gained a query |
| 2 `feat(policy): four rules` | `policy/rules/{min_release_age,install_scripts,typosquat,osv_severity}.rs`, `policy/distance.rs`, `policy/lists/*` + `known/*` + `NOTICE`, `scripts/toplists.sh` (Go major-suffix normalisation, `--known` writing the known and holdout tiers), `telemetry/vulns/{mod,osv}.rs` (`assess_batch`, permit in `query_batch`), `policy/{mod,writer}.rs` (`all_rules`, `evaluate_all` in the gather task, `evaluate_batch` in the spawned flush), `policy/startup.rs` (`inapplicable` OCI rules) | unit tests per rule incl. unknown date, `@babel/core`, `redis/v9`, short names, `requests`, `is-array`, `mssql`, the holdout false-positive sweep under its ceiling; verdict asserts added to commit-1 cases (`min_release_age = would_block` / `unknown` in the npm and cargo cases, `unknown` twice in `oci_index_without_child_is_unknown_after_ttl`, `not_applicable` pairs in the OCI HEAD case, `install_scripts` in the stem and gating cases); `osv_rule_uses_fake_osv_and_id_cache` (one POST per flush), `flush_never_blocks_receive`, `npm_install_scripts_from_cached_packument`, `startup::notes_name_recording_and_inapplicable_members` |
| 3 `feat(policy): report API, erasure and retention` | `api/policy.rs`, `api/mod.rs`, `server.rs` (four routes), `policy/store.rs` (report queries, `subject` filter, `delete_by_user`), `telemetry/cleanup.rs` (`sweeps_configured`, policy sweep), `config.rs` (`policy_report_older_than_days`), `docs/api.md`, `README.md` (`[cleanup]`, `[policy.<repo>]`, the recording, self-service and erasure sentences), `Makefile` | report asserts in every format case (incl. the `rule=install_scripts` tile/chip agreement), `rules_endpoint_reports_effective_config`, `erase_user_deletes_rows_and_audits` (no name in the audit row, the homonymous token untouched), `me_policy_shows_only_own_rows` (the `alice`-named token of `bob`), `cleanup::{sweeps_configured_by_policy_bound_alone, policy_sweep_deletes_old_rows}` |
| 4 `feat(web): policy report page` | `frontend/src/core/{types,api}.ts`, `core/stores/{live,live.test}.ts` (`maxWait`), `core/stores/{policy,policy.test}.ts`, `pages/admin/PolicyReport.tsx`, `index.tsx`, `components/Sidebar.tsx`, `src/web/mod.rs`, `examples/policy-demo.toml` | `pnpm test`, `pnpm lint`, `cargo test --test policy_test`; manual run of section 10 including the `npm ci` variant |

Rules of the road (proxy design, unchanged): no function over 80 lines, no new file over ~400 lines, minimal comments, `-D warnings`.

## 12. Out of scope and risks

- Audit entries for policy config changes: config file only, no API writes, nothing to audit (requirement f). Erasure is an API write and is audited (count only).
- Follow-ups: a request-level event kind recording upstream misses (the only way a 404ing OCI or npm typosquat could be seen), client IP/User-Agent (a `Cx` change), typosquat
  letter insertion/deletion and distance 2 once a week of `rule=typosquat` traffic gives a false-positive rate, an `install_scripts` allowlist, actor redaction, an erasure button.
- Format limits. Go records `.zip` downloads, not resolutions: MVS walks `@v/list` and `.mod` for every module in the graph and fetches a `.zip` only for modules whose packages are built
  (`FileLeaf::proxy` records `FileKind::Zip` alone, `go/leaves.rs:156-173`), so a Go member reports fewer rows than it resolves. OCI has one live rule: `osv_ecosystem()` is `None`
  (`db/kinds.rs:75-82`), `typosquat` is `not_applicable`, `install_scripts` is npm-only, leaving `min_release_age`; startup warns when the other two are on for an OCI member. PyPI is not a gap: `supports_kind` refuses `Proxy | Group` (`db/kinds.rs:84-86`).
- Recorder cost: the extra requests go through the member's proxy cache and singleflight, so N clients pulling one version share one request; but the npm packument is per package per
  `default_ttl` (24 h), full-size, up to 100 MB (section 4): an `npm ci` fleet on a cold packument cache pays one packument per package per day that it never paid before, plus one
  conditional refresh per package per 5 min while a version is newer than the copy, unless the rules that need it are off or `fetch_missing_facts = false`; reading it is one full parse
  per packument row, memoised and coalesced (`npm_facts`, a `OnceCell` per row on `spawn_blocking`), ~1 s of CPU once a day for the largest, not once per tarball nor once per concurrent tarball. OCI costs Docker Hub nothing metered: Hub counts `/v2/*/manifests/*` GETs
  as pulls and the recorder issues none (an index is dated from the child the client pulled, a manifest from its own body); the config blob GET is unmetered and singleflighted with the
  client's own; the anonymous 100 pulls / 6 h stay the client's. The OCI date is the publisher's build stamp, not the push time (section 5.1). Cargo dates come from the crates.io API
  alone, paced at one request per second per host with a 15-deep waiting line (`gather_timeout / pacer_period`) and a `gather_timeout` deadline on the wait itself (section 4): a cold `cargo build`
  of 300 crates dates at most 15 rows, fewer when crates.io is slow (a holder keeps the lane for `pacer_period` plus its latency), and marks the rest `rate-limited` (`unknown`) until they are
  pulled again, at once past the cap and after 15 s in line; no slot is held longer than 2 x `gather_timeout`; the pace is the crawler policy, not a tunable, and a 429 stops all lookups on that host for 60 s. OSV is off the request
  path: one `querybatch` per ecosystem per flush of up to 64 rows, deduplicated by the memo, under the scanner's `max_concurrency` permit that `query_batch` acquires from this change on (`osv.rs:212`); an outage is `unknown`, never a gap.
- Queue overflow: the writer serves 320 cold events/s steady state (`INFLIGHT = 64` slots at 200 ms facts; the load test's floor) and thousands/s warm (memo hits; the first
  event per npm packument row parses it); `QUEUE = 4096` absorbs a burst of 500/s for 10 s without a drop; both are constants to raise. The bus sees at most two
  `policy.resolution` frames a second **per `(requested_repo, member_repo)` pair** whatever the volume (section 3): a group over three proxy members addressed directly and through the group is six pairs,
  twelve frames a second at worst against the 256-deep `broadcast` (`src/events.rs:57`), so recording never turns downloads into WebSocket `resync` storms. Beyond that events drop, visible in `opencargo_policy_dropped_total`, `process.dropped_since_start` and the page banner: never silent. One writer is one SQLite connection.
- Recording is personal data (actor label, artifact, time, keyed by `user_id`) held 90 days by default, visible to its subject at `/api/v1/me/policy`, erasable per user with an
  audit entry that keeps the count, not the name; off until a rule is enabled, announced by one startup `warn`, documented in `README.md`. Identity is `user_id`, never a string a user can choose.
- Lists age; `scripts/toplists.sh` regenerates both tiers, the headers record the date; a missing top name costs a false negative, a missing known name a false positive on a package below the
  top 20 k one edit from a top-5 k name, the residual section 5.4 measures on the holdout tier (~1 MB of `include_str!` for the three known lists; the holdout is test-only). Licences live in
  `policy/lists/NOTICE`; the crates.io dump is not committed until its licence is confirmed. A squat that *adds* a hyphen to a top name is missed (section 5.4), the price of never flagging `is-array`.

## 13. As built

Deviations accepted by review, per delivery-plan row, then the whole-feature review outcome. Where this section differs from sections 3 to 11, this section wins.

### Row 1 (`2796186`)
- Schema: policy_resolutions gains a `date_source TEXT NOT NULL DEFAULT ''` column (section 6 has none). The design's own HTTP-driven cases assert date_source (fetch / refresh / not-in-packument / rate-limited / index-unpulled / unset-created ...) and without a column those claims are unobservable from the DB; store::insert_batch writes it, the harness PolicyRow reads it.
- OCI child-key semantics (section 4): a pure FIFO pop cannot satisfy the design's own case (20 amd64 pulls then one arm64 pull: the arm64 key's front entry is a released seq, so the 21st index would expire index-unpulled; and B's bare amd64 pull 1 s after A's pull would be swallowed by pull 20's leftover). Implemented instead: a child spends the first entry whose index is still parked (FIFO among parked), a key with none left is a sibling of a released pull (skopeo --all stays one pull), and releasing seq s supersedes every entry with seq < s for the same (member, name). Pinned by writer::sibling_of_released_pull_is_suppressed_until_a_later_release and the harness index case; the sentinel in that case is a plain OCI manifest, not an npm pull (same FIFO proof, one fewer upstream).
- npm memo stamp is (row id, fetched_at, digest) instead of (id, fetched_at): fetched_at has second granularity, so a refresh landing in the same second as the fetch would keep stale parsed facts; the body digest changes iff the body did. PackageFacts::knows treats a version present but lacking time[version] as a miss (risk checklist: a peeked packument lacking time[version] falls through to a fetch).
- date_source gains two values outside section 4's list: `not-found` (upstream 404 on a recorder fetch, e.g. a crates.io version the API does not know) and `failed` (transport/5xx/unreadable cache), logged at debug; the closed list had no honest name for either and `fetch` must never label an undated row.
- PolicyEngine::new / new_tuned take no `scanner` argument in this commit: no code reads it until commit 2's osv_severity batch, and an unused field/parameter would be dead code; commit 2 adds it (one-line change in tests/common spawn_in). record_now is likewise not implemented: no commit-1 test drives it (the writer tests drive the real writer through `unspawned`).
- policy/facts.rs is a directory module (facts/mod.rs, facts/npm.rs, facts/oci.rs) and the larger test modules live in sibling files via #[path] (policy/tests.rs, policy/testing.rs, writer_tests.rs, facts/npm_tests.rs) to keep every new source file under ~400 lines; module paths named by the design (facts::*, policy::writer::tests::*, facts::npm::tests) are unchanged. tests/policy_test.rs is 660 lines as the single home of the cases (same precedent as group_resolver_test.rs).
- startup_notes already computes `inapplicable` (OCI member with typosquat/osv_severity on) and build_state warns on it; the design lists that under commit 2 but it is pure config with the flags already in PolicyConfig, and it lets the named test notes_name_recording_and_inapplicable_members exist now.
- wait_for_policy_rows polls up to 10 s (design: 5 s): the paced cargo case needs 6 x (500 ms + 100 ms latency) before its rows exist. Makefile gains test-load (named in this row's proof column) and adds --test policy_test to test-quick (listed under commit 3).
- VersionMeta's cache kind is `cargo-meta` (design: `meta`) to match the existing `cargo-*` naming; the fake cargo index's config.json now serves `api` = its own base URL instead of `""` (existing tests only assert the rewritten copy). Fixture (engine/fixture.rs) gains `starts` and `status` fields for the pacer tests instead of a packument route.
- engine: refresh maps an exchange Err (cap/digest/integrity) to Ok(NotFound) with a warn, per 'everything else is NotFound'; fetch's metric calls are untouched, refresh records no cache hit/miss metric.

### Row 2 (`7b773d5`)
- all_rules(scanner: Arc<VulnScanner>, memo: Arc<OsvMemo>) takes two arguments (design: all_rules()): Rule::evaluate is sync and osv_severity needs the scanner (disabled check) and the shared memo for its hit; Shared holds the same Arcs (scanner, osv_memo) and PolicyEngine::new/new_tuned/unspawned gained the scanner parameter as section 3 specifies (server.rs and tests/common spawn_in pass state.vuln_scanner).
- OsvFinding.at is a DateTime<Utc> checked against the rule's `now` argument (design: Instant): memo_hit_is_not_deferred proves expiry by passing now + 2h instead of subtracting an hour from a monotonic clock, which can underflow on a freshly booted CI host.
- Tuning gained flush_period (default 100 ms, replacing the writer's TICK const) and the writer restarts its tick when it wakes from idle, so the first flush after idleness has a full period to fill instead of writing a one-row batch at once. The harness case osv_rule_uses_fake_osv_and_id_cache sets it to 2 s and asserts batches == [1, 64, 6]: with a fixed 100 ms tick the 70 concurrent pulls spread across ticks (SQLite serialises the 70 cache writes over ~200-300 ms) and produced 3-5 POSTs on every run, so the design's 'at most 2 POSTs' claim could not be made exact without the knob.
- List sources differ from section 5.4's table where the named source was unreachable: crates.txt comes from the crates.io API (GET /api/v1/crates?sort=downloads, 50 pages paced at one request per second with an identifying User-Agent) instead of the 400 MB database dump (same data); go.txt and all three known/ and holdout/ tiers come from packages.ecosyste.ms dependents rankings (CC BY-SA 4.0) instead of the deps.dev BigQuery dataset, which needs a GCP account. npm.txt is npm-high-impact topDownload as designed (MIT). NOTICE records sources, licences and that crates.io publishes no explicit data licence; scripts/toplists.sh regenerates everything (--known writes the known and holdout tiers). Holdout sizes: npm 19 983, crates 20 000, go 17 774 (Go major-suffix dedupe over 40 000 ranks).
- The typosquat top pool is bucketed by (length, first bigram) and (length, last bigram) rather than scanned linearly with a length prefilter: an edit touches at most two adjacent positions so a 5+ character name keeps one bigram; same verdicts, and it keeps the 60 000-name holdout and sibling sweeps under a second in debug builds. one_edit works on bytes.
- Harness OSV case proves the memo as pull -> wait for the row -> two more pulls (osv.batches() == [1]) rather than three back-to-back pulls; a within-flush dedupe of the same triple is proven in-crate by batch_groups_by_ecosystem_and_dedupes (64 npm rows of 3 versions + 3 cargo rows = 2 POSTs of 3 queries).
- cargo_row_dates_from_api_or_unknown now dates widget 30 minutes before now (was a fixed 2026-03-01) so the cargo case carries the would_block the row-2 proof asks for ('published 30m ago, threshold 1h'); the not-found crate is the unknown.
- Rule::evaluate does not re-check enabled(): evaluate_all filters, as section 5 says; the *_not_applicable unit tests therefore do not assert None on a default config, and a new rules::tests::only_enabled_rules_leave_a_slot pins the filtering and the rule order.
- Additions outside the design's listings: Age::approx(Duration) for the 'published 2h ago' reasons; RuleVerdict::new and PartialEq on RuleVerdict; Severity::as_str/Display; VulnScanner::enabled(); details_per_dep shared by assess and assess_batch in vulns/mod.rs; engine_with/scanner and an in-crate FakeOsv in policy/testing.rs (cfg(test)); policy_verdicts/verdict_of/rules_of and FakeOsv::batches in the harness.
- Not in this row and untouched: policy/startup.rs already carried the inapplicable OCI notes and its test from commit 1; Makefile, docs/design/*, plan-*.md untouched.

### Row 3 (`da387b2`)
- Major (parse_since panic): fixed with `from_std(..).ok().and_then(|d| now.checked_sub_signed(d)).ok_or_else(400 'invalid since ..: age out of range')`; chose the explicit 400 over clamping to MIN_UTC so an absurd value is visibly rejected rather than silently meaning 'everything'.
- Minor (sargable retention): delete_older_than now reads `created_at < datetime('now', '-' || ?1 || ' days')`; policy_sweep_deletes_old_rows unchanged and still green.
- Minor (process counter): render() emits `process.dropped_since_start` only when subject.is_none(), i.e. on the admin report; /me/policy no longer carries it. docs/api.md sentence adjusted to say so and to document the 400 on an out-of-range age (docs/api.md is not a protected file; docs/design/* untouched).
- Minor (rustfmt): only policy_days was reshaped; the new cleanup.rs test block was already fmt-clean, pre-existing hunks left alone as the review asked.
- Commit body extended with the why of the three behaviour changes and names the new test; title kept verbatim. Nothing pushed.

### Row 4 (`7cca4f0`)
- Major (CSS specificity): fixed by naming `.stats-grid.cols-3` alongside `.stats-grid` inside both the 1080px and 480px media blocks in global.css; the earlier deviation claim was wrong and is now verified in-browser at 1400/1080/480/360px.
- Minor (Older button): ReportTable now takes `size` from the report body (`d().size`) and disables Older when `entries.length < size`; the client constant PAGE_SIZE is removed (no other reader existed).
- Minor (osv_enabled): `enabledRules` adds osv_severity only when a member sets it AND `rules.osv_enabled` is true, so the filter labels it '(off)' when the scanner is disabled; covered by the extended enabledRules test.
- Minor (since in empty state): added pure `isFiltered({since, repo, rule})` in policy.ts (default since exposed as DEFAULT_SINCE) plus a `filtered()` accessor on the store; EmptyReport uses `store.filtered()`; tested at helper and store level.
- Not addressed, pre-existing: `prettier --check frontend/src/styles/global.css` already fails on the committed file at the parent commit (unrelated to the two lines changed here), so it was left alone.
- Scope gaps not re-attested: the section 10 manual run (steps 3-5, npm ci variant) was not re-run in this fix pass; the render-level banner test remains at the helper level since vitest has no solid JSX environment; the @acme/* scope on npm-hosted in examples/policy-demo.toml stays descriptive (no config key exists); the duplicated ws bus mock between live.test.ts and policy.test.ts stays (vi.mock hoisting is per file).

### Whole-feature review (three lenses, double refutation)

12 findings confirmed and fixed in follow-up commits:
- high (false-positives): Family-prefix substitutions flag whole crate families as typosquats (sha1-asm, git-*/gix-*, fp-/sp-, wdk-sys)
- medium (false-positives): Digit substitutions flag version-numbered siblings (bzip3 vs bzip2, soup2 vs soup3, murmur2 vs murmur3)
- medium (false-positives): `-` <-> `.` substitution flags real npm sibling names although separator *insertion* is deliberately exempt
- critical (cost): Report totals scan the whole `since` window uncapped, and the page refetches them ~2×/s
- high (cost): Retention sweep is one DELETE with FK cascade: 85 s of held SQLite write lock, every other write fails
- high (cost): npm `refresh_floor` never engages when the refresh itself rewrites the cache row: one full packument GET per tarball
- medium (cost): Recorder-initiated `refresh` holds the client-facing singleflight guard on a fresh packument row
- medium (cost): Writer receive loop head-of-line blocks on the INFLIGHT semaphore, stalling flushes, parked indexes and WS notifies
- low (cost): Every OCI manifest body is read from disk and JSON-parsed twice, the first time on the writer's single receive loop
- high (contract): Recorder-initiated fetches still write negative cache rows, so recording can 404 clients
- medium (contract): The writer's receive loop awaits the inflight semaphore, so 64 slow gathers stall classification, flush, index release and the live event
- low (contract): `osv_severity = "unknown"` is accepted and flags every advisory as would_block

Fix report:

Commits on `feat/policy-report` (all gated with `cargo clippy --all-targets --all-features -- -D warnings && cargo test`, plus `pnpm test`/`pnpm lint` for the page; each new test was verified to fail with the fix reverted):

| commit | finding | proof |
|---|---|---|
| `e33c2e3 fix(policy): bound the report totals` | critical: totals scan + 2×/s refetch | `policy::totals::{refetches_between_flushes_never_rescan_the_window, erasure_forgets_the_snapshot}`; frontend `refetches at a floor rate under a stream of flushes`, `events during an in-flight refetch cost one more refetch after it lands` |
| `c41c627 fix(policy): typosquat reads family markers as siblings` | high family prefix + medium digit + medium `-`/`.` | `distance::one_edit_table` (14 new rows), `typosquat::family_markers_pass`; holdout residual 82/138/5 → 67/101/5 (52 fewer, as measured), ceilings lowered to 75/110/10 |
| `12989f4 fix(policy): recorder requests never write a negative row` | high | `facts::missing_fact_never_writes_a_negative_row` (npm + cargo), `tests/policy_test.rs::recorder_miss_never_404s_the_client` |
| `f5c3edd fix(policy): npm refresh floor is per package, not per row` | high | `facts::npm::refresh_floor_survives_a_rewritten_row`; `npm_version_newer_than_cached_packument_refreshes` extended |
| `a945d48 fix(policy): retention and erasure delete in chunks` | high | `store::{retention,erasure}_deletes_chunk_by_chunk_with_verdicts_explicit` (FK off, `DELETE_CHUNK + 1` rows) |
| `2baf119 fix(policy): refresh locks its own singleflight namespace` | medium | `engine::tests::refresh_never_blocks_a_fresh_hit` |
| `472c6d0 fix(policy): writer keeps receiving while every slot is held` | both medium writer findings | `writer::full_slots_never_stall_the_tick` |
| `0c7af91 fix(policy): OCI body parsed once at classification` | low | `facts::oci_gather_dates_from_the_classified_body_without_a_read` |
| `f151fc3 fix(policy): reject osv_severity = "unknown" at config load` | low | `rules::config_rejects_unknown_severity` |

Design deviations (docs/design/policy-report.md untouched; to record there):
- §6 store API: `report_totals(pool, f, IdRange)` + `store::max_id`; new `src/policy/totals.rs` (`TotalsCache`: snapshot per filter key, delta on `id > upto`, full scan no sooner than 5 s and 10× its own cost; `PolicyEngine::{totals, forget_totals}`). Migration 014: `idx_policy_res_created` dropped for covering `idx_policy_res_created_flags(created_at, would_block, unknown)`, new `idx_policy_verdicts_res(resolution_id, rule, verdict)`. `delete_older_than`/`delete_by_user` chunk at `DELETE_CHUNK = 5000`, verdicts deleted explicitly. Store tests live in `store_tests.rs`.
- §3 engine: new `ProxyEngine::observe` (fetch's lookup/singleflight/exchange, `Miss::Ignore`); the cold packument, go `.info`, OCI blob and paced crates.io lookup use it; `refresh` locks under `refresh/…`; cache hit/miss metrics count client fetches only.
- §3 writer: an event with no free slot waits in a local queue behind a permit branch of the `select!` instead of the recv arm awaiting the permit; the channel still bounds and drops.
- §3/§4 `Source::Oci` gains `parsed: Option<Value>`; `oci_published_at` takes the dated body from `dated_body`.
- §4 npm: the refresh floor is per `(member, name)` (carried across a rewritten row), so §9's `npm_version_newer_than_cached_packument_refreshes` is now "3 packument requests for 5 pulls" with `policy_tuning.refresh_floor = 300 ms` (absent stem inside the floor: no request; past it: one 304).
- §5.4: digit substitutions, separator-for-separator swaps and a differing leading `-` token of ≤3 chars are family markers, never edits; `known_sibling_passes` proves `base65` passes on the digit rule alone and flags `basf64` instead (design named `base65` as a would_block).
- §8 UI: debounce 1000 ms / maxWait 3000 ms (was 300/2000); `useLive` coalesces events arriving during an in-flight refetch.
- `PolicyConfig.osv_severity` has a `deserialize_with` rejecting `unknown`; README lists the four levels.

Deliberately not done:
- A separate per-filename negative memo for absent npm stems: within the floor the carried refresh cell already answers without a request; past it one conditional request per package per floor is the design's own contract.
- The retention sweep does not invalidate totals snapshots (`run_cleanup` has no engine handle): a snapshot can over-count for at most its TTL, and only for windows ≥ the 90-day retention bound; erasure does invalidate.
- `verdict()` was not additionally guarded on `level > Unknown`: the threshold is refused at config load, which makes the `Some((id, Unknown)) => Pass` arm live.
- `cargo fmt --check` fails on pre-existing files outside this series (e.g. `src/api/dashboard.rs`); only the files this series touched were formatted.
